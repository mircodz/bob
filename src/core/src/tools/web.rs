//! Web tools: web_fetch (retrieve a URL as readable text) and web_search (a
//! keyless query against DuckDuckGo's HTML endpoint).

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_string(),
            description:
                "Fetch a URL over HTTP(S) and return its readable text content (HTML is stripped \
                to plain text). Use this to read documentation, issues, or pages the user links. \
                You must have a specific URL — this does not search the web. Output is truncated \
                for very large pages."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_chars": { "type": "number", "description": "Truncate output to this many chars (default 20000)." }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let url = input["url"].as_str().unwrap_or("");
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ToolError::invalid_input(
                "url must start with http:// or https://",
            ));
        }
        let cap = input["max_chars"].as_u64().unwrap_or(20_000) as usize;

        let res = match reqwest::get(url).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        if !res.status().is_success() {
            return Err(ToolError::failed(format!(
                "HTTP {} fetching {}",
                res.status().as_u16(),
                url
            )));
        }
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match res.text().await {
            Ok(b) => b,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        let text = if content_type.contains("html") {
            html_to_text(&body)
        } else {
            body
        };
        if text.chars().count() > cap {
            let truncated: String = text.chars().take(cap).collect();
            Ok(format!("{}\n…[truncated]", truncated))
        } else {
            Ok(text)
        }
    }
}

/// `web_search`: a keyless web search via DuckDuckGo's HTML endpoint. Returns a
/// ranked list of results (title · url · snippet) the model can then `web_fetch`.
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".to_string(),
            description:
                "Search the web and return a ranked list of results (title, URL, and a short \
                snippet). Use this when you need current information or don't already have a URL — \
                then call web_fetch on the most relevant result to read the full page. Good for \
                docs, error messages, library/API changes, and recent events. Returns titles + \
                URLs + snippets, not full page text."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query." },
                    "max_results": { "type": "number", "description": "How many results to return (default 8, max 20)." }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let query = input["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return Err(ToolError::invalid_input("query is required"));
        }
        let limit = (input["max_results"].as_u64().unwrap_or(8) as usize).clamp(1, 20);

        // DuckDuckGo's no-JS HTML endpoint returns server-rendered results we can
        // scrape without an API key. A browser-like UA avoids being turned away.
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (compatible; bob/0.1; +https://example.invalid/bob)")
            .build()
            .map_err(|e| ToolError::failed(e.to_string()))?;
        let res = client
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query)])
            .send()
            .await
            .map_err(|e| ToolError::failed(format!("search request failed: {}", e)))?;
        if !res.status().is_success() {
            return Err(ToolError::failed(format!(
                "search returned HTTP {}",
                res.status().as_u16()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ToolError::failed(e.to_string()))?;

        let results = parse_ddg_results(&body, limit);
        if results.is_empty() {
            return Ok(format!("no results for \"{}\".", query));
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
            if !r.snippet.is_empty() {
                out.push_str(&format!("   {}\n", r.snippet));
            }
        }
        Ok(out.trim_end().to_string())
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Extract results from DuckDuckGo's HTML page. Each result is an anchor with
/// class `result__a` (title + href, the href being a redirect wrapper we unwrap),
/// followed by a `result__snippet` block.
fn parse_ddg_results(html: &str, limit: usize) -> Vec<SearchResult> {
    // Anchor: <a ... class="result__a" href="LINK">TITLE</a>
    let anchor = Regex::new(r#"(?is)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .expect("valid regex");
    // Snippet: <a ... class="result__snippet" ...>TEXT</a>
    let snippet =
        Regex::new(r#"(?is)class="result__snippet"[^>]*>(.*?)</a>"#).expect("valid regex");

    let snippets: Vec<String> = snippet
        .captures_iter(html)
        .map(|c| clean_fragment(&c[1]))
        .collect();

    let mut out = Vec::new();
    for (i, cap) in anchor.captures_iter(html).enumerate() {
        if out.len() >= limit {
            break;
        }
        let url = unwrap_ddg_url(&cap[1]);
        let title = clean_fragment(&cap[2]);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = snippets.get(i).cloned().unwrap_or_default();
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out
}

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<encoded>`; pull the
/// real destination out of the `uddg` query param and percent-decode it.
fn unwrap_ddg_url(raw: &str) -> String {
    let raw = raw.trim();
    let candidate = if let Some(idx) = raw.find("uddg=") {
        let rest = &raw[idx + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        percent_decode(&rest[..end])
    } else {
        raw.to_string()
    };
    if let Some(stripped) = candidate.strip_prefix("//") {
        format!("https://{}", stripped)
    } else {
        candidate
    }
}

/// Minimal percent-decoding (enough for URLs in DDG's `uddg` param).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Decode `%XX` by reading the two following BYTES directly. Slicing the
        // &str as `&s[i+1..i+3]` panics when a `%` is followed by a multi-byte
        // UTF-8 char (the slice lands off a char boundary) — reachable from
        // untrusted search-result HTML, so it must never panic.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// One hex digit (ASCII) → its value, or None if not a hex digit.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip HTML tags + decode common entities from a small result fragment.
fn clean_fragment(frag: &str) -> String {
    let tags = Regex::new(r"(?s)<[^>]+>").expect("valid regex");
    let s = tags.replace_all(frag, "");
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_to_text(html: &str) -> String {
    let script = Regex::new(r"(?is)<script.*?</script>").unwrap();
    let style = Regex::new(r"(?is)<style.*?</style>").unwrap();
    let tags = Regex::new(r"(?s)<[^>]+>").unwrap();
    let ws = Regex::new(r"[ \t]+").unwrap();
    let blanks = Regex::new(r"\n\s*\n\s*\n").unwrap();

    let s = script.replace_all(html, "");
    let s = style.replace_all(&s, "");
    let s = tags.replace_all(&s, " ");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let s = ws.replace_all(&s, " ");
    let s = blanks.replace_all(&s, "\n\n");
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_ddg_redirect_url() {
        let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&rut=abc";
        assert_eq!(unwrap_ddg_url(raw), "https://doc.rust-lang.org/std/");
    }

    #[test]
    fn parses_result_anchor_and_snippet() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">
              Example <b>Title</b>
            </a>
            <a class="result__snippet" href="x">A short <b>snippet</b> here.</a>
        "#;
        let results = parse_ddg_results(html, 8);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].title, "Example Title");
        assert_eq!(results[0].snippet, "A short snippet here.");
    }

    #[test]
    fn clean_fragment_strips_tags_and_entities() {
        assert_eq!(clean_fragment("a &amp; <b>b</b>  c"), "a & b c");
    }

    #[test]
    fn percent_decode_handles_normal_and_multibyte_without_panicking() {
        // Normal decoding still works.
        assert_eq!(percent_decode("https%3A%2F%2Fx"), "https://x");
        // Regression: a `%` immediately before a multi-byte UTF-8 char used to
        // panic (`&s[i+1..i+3]` off a char boundary). Must pass the `%` through.
        assert_eq!(percent_decode("%a€x"), "%a€x");
        // A bare trailing `%` and a `%` with a non-hex follower are passed through.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
