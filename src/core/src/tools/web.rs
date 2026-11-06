use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

pub const MAX_WEB_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_WEB_PREVIEW_CHARS: usize = 200_000;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebRequest {
    Fetch { url: String, max_chars: usize },
    Search { query: String, max_results: usize },
}

impl WebRequest {
    pub fn prepare(name: &str, input: &Value) -> anyhow::Result<Self> {
        let fields = input
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("web input must be an object"))?;
        let number = |name: &str, default: usize, maximum: usize| -> anyhow::Result<usize> {
            let value = match fields.get(name) {
                None => default,
                Some(value) => value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| anyhow::anyhow!("{name} must be a positive integer"))?,
            };
            anyhow::ensure!(
                (1..=maximum).contains(&value),
                "{name} must be in 1..={maximum}"
            );
            Ok(value)
        };
        match name {
            "web_fetch" => {
                anyhow::ensure!(
                    fields
                        .keys()
                        .all(|key| ["url", "max_chars"].contains(&key.as_str())),
                    "unknown web_fetch field"
                );
                let raw = input["url"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("url is required"))?;
                anyhow::ensure!(raw.len() <= 16_384, "URL exceeds 16384 bytes");
                let url =
                    reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid HTTP(S) URL"))?;
                anyhow::ensure!(
                    ["http", "https"].contains(&url.scheme()) && url.host_str().is_some(),
                    "url must start with http:// or https://"
                );
                anyhow::ensure!(
                    url.username().is_empty() && url.password().is_none(),
                    "URL credentials are not supported"
                );
                Ok(Self::Fetch {
                    url: url.into(),
                    max_chars: number("max_chars", 20_000, MAX_WEB_PREVIEW_CHARS)?,
                })
            }
            "web_search" => {
                anyhow::ensure!(
                    fields
                        .keys()
                        .all(|key| ["query", "max_results"].contains(&key.as_str())),
                    "unknown web_search field"
                );
                let query = input["query"].as_str().unwrap_or("").trim();
                anyhow::ensure!(
                    !query.is_empty() && query.len() <= 16_384,
                    "query must contain 1..=16384 bytes"
                );
                Ok(Self::Search {
                    query: query.into(),
                    max_results: number("max_results", 8, 20)?,
                })
            }
            _ => anyhow::bail!("unknown web operation"),
        }
    }
}

pub struct WebResponse {
    pub text: String,
    pub preview: String,
}

pub struct WebClient {
    client: reqwest::Client,
}

impl WebClient {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; bob/0.1)")
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()?,
        })
    }

    pub async fn execute(&self, request: &WebRequest) -> anyhow::Result<WebResponse> {
        let response = match request {
            WebRequest::Fetch { url, .. } => self.client.get(url).send().await,
            WebRequest::Search { query, .. } => {
                self.client
                    .post("https://html.duckduckgo.com/html/")
                    .form(&[("q", query)])
                    .send()
                    .await
            }
        }
        .map_err(|error| anyhow::anyhow!("web request failed: {}", error.without_url()))?;
        anyhow::ensure!(
            response.status().is_success(),
            "web request returned HTTP {}",
            response.status().as_u16()
        );
        let html = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("html"));
        let bytes = read_bounded_body(response).await?;
        Ok(decode_web_response(request, &bytes, html))
    }
}

fn decode_web_response(request: &WebRequest, bytes: &[u8], html: bool) -> WebResponse {
    let body = String::from_utf8_lossy(bytes);
    let (text, cap) = match request {
        WebRequest::Fetch { max_chars, .. } => (
            if html {
                html_to_text(&body)
            } else {
                body.into_owned()
            },
            *max_chars,
        ),
        WebRequest::Search { query, max_results } => {
            let results = parse_ddg_results(&body, *max_results);
            let mut out = String::new();
            for (i, result) in results.iter().enumerate() {
                out.push_str(&format!("{}. {}\n   {}\n", i + 1, result.title, result.url));
                if !result.snippet.is_empty() {
                    out.push_str(&format!("   {}\n", result.snippet));
                }
            }
            if out.is_empty() {
                out = format!("no results for \"{query}\".");
            }
            (out.trim_end().to_owned(), 20_000)
        }
    };
    let mut preview: String = text.chars().take(cap).collect();
    if preview.len() < text.len() {
        preview.push_str("\n…[preview truncated; full response retained]");
    }
    WebResponse { text, preview }
}

async fn read_bounded_body(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_WEB_BODY_BYTES as u64),
        "web response exceeds {MAX_WEB_BODY_BYTES} bytes"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow::anyhow!("web response failed: {}", error.without_url()))?
    {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_WEB_BODY_BYTES,
            "web response exceeds {MAX_WEB_BODY_BYTES} bytes"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

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
            Ok(format!("{}\n…[truncated]", truncated).into())
        } else {
            Ok(text.into())
        }
    }
}

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

        // The keyless HTML endpoint requires a browser-like User-Agent.
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
            return Ok(format!("no results for \"{}\".", query).into());
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
            if !r.snippet.is_empty() {
                out.push_str(&format!("   {}\n", r.snippet));
            }
        }
        Ok(out.trim_end().into())
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// DDG pairs `result__a` anchors with `result__snippet` blocks in result order.
fn parse_ddg_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let anchor = Regex::new(r#"(?is)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .expect("valid regex");
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

/// DDG wraps destinations in `//duckduckgo.com/l/?uddg=<encoded>`.
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

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Use bytes: untrusted `%` sequences can split UTF-8 character boundaries.
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

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

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
    use crate::test_support::{files::tool_context, TestDir};

    #[test]
    fn durable_web_validation_and_preview_preserve_full_unicode_content() {
        for input in [
            json!({"url":"file:///private"}),
            json!({"url":"https://user:secret@example.test"}),
            json!({"url":"https://example.test","max_chars":0}),
            json!({"url":"https://example.test","max_chars":200001}),
        ] {
            assert!(WebRequest::prepare("web_fetch", &input).is_err());
        }
        for input in [
            json!({"query":" "}),
            json!({"query":"test","max_results":21}),
            json!({"query":"test","max_results":1.5}),
        ] {
            assert!(WebRequest::prepare("web_search", &input).is_err());
        }
        let request = WebRequest::prepare(
            "web_fetch",
            &json!({"url":"https://example.test","max_chars":2}),
        )
        .unwrap();
        let response = decode_web_response(&request, "<p>界😀abc</p>".as_bytes(), true);
        assert_eq!(response.text, "界😀abc");
        assert_eq!(
            response.preview,
            "界😀\n…[preview truncated; full response retained]"
        );
        let search =
            WebRequest::prepare("web_search", &json!({"query":"fixture","max_results":1})).unwrap();
        let response = decode_web_response(&search, br#"<a class="result__a" href="https://example.test/">Fixture</a><a class="result__snippet">A snippet</a>"#, true);
        assert_eq!(
            response.text,
            "1. Fixture\n   https://example.test/\n   A snippet"
        );
    }

    #[tokio::test]
    async fn durable_web_body_reader_rejects_oversize_and_can_be_cancelled() {
        use crate::test_support::http::response;
        let (body, _release) = response("text/plain", "fixture", false).await;
        assert_eq!(read_bounded_body(body).await.unwrap(), b"fixture");
        let bytes = "x".repeat(MAX_WEB_BODY_BYTES + 1);
        let (body, _release) = response("text/plain", &bytes, false).await;
        assert!(read_bounded_body(body)
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
        let (body, release) = response("text/plain", "partial", true).await;
        let mut request = Box::pin(read_bounded_body(body));
        assert!(futures::poll!(&mut request).is_pending());
        drop(request);
        drop(release);
    }

    #[tokio::test]
    async fn invalid_fetch_schemes_and_blank_searches_fail_before_network_io() {
        let dir = TestDir::new();
        let ctx = tool_context(dir.path());
        for input in [
            json!({}),
            json!({"url": "file:///not-read"}),
            json!({"url": "ftp://example.invalid"}),
        ] {
            let error = WebFetchTool.execute(input, &ctx).await.unwrap_err();
            assert_eq!(
                error.kind,
                crate::tools::registry::ToolErrorKind::InvalidInput
            );
            assert!(error
                .message
                .contains("url must start with http:// or https://"));
        }
        for input in [json!({}), json!({"query": " \n\t "})] {
            let error = WebSearchTool.execute(input, &ctx).await.unwrap_err();
            assert_eq!(
                error.kind,
                crate::tools::registry::ToolErrorKind::InvalidInput
            );
            assert!(error.message.contains("query is required"));
        }
    }

    #[test]
    fn unwraps_ddg_redirect_url() {
        for (raw, expected) in [
            (
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&rut=abc",
                "https://doc.rust-lang.org/std/",
            ),
            (" //example.com/path ", "https://example.com/path"),
            ("https://example.com/a?q=a+b", "https://example.com/a?q=a+b"),
            ("//duckduckgo.com/l/?uddg=", ""),
        ] {
            assert_eq!(unwrap_ddg_url(raw), expected);
        }
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
    fn percent_decode_handles_normal_and_multibyte_without_panicking() {
        for (input, expected) in [
            ("https%3A%2F%2Fx", "https://x"),
            ("%a€x", "%a€x"),
            ("100%", "100%"),
            ("%zz", "%zz"),
            ("%2", "%2"),
            ("%c3%a9+ok", "é+ok"),
            ("%FF", "�"),
        ] {
            assert_eq!(percent_decode(input), expected);
        }
    }

    #[test]
    fn result_limit_counts_valid_anchors_and_missing_snippets_are_empty() {
        let html = r#"
            <a class="result__a" href="">discarded</a>
            <a class="result__a" href="https://example.com/empty"><b> </b></a>
            <a class="result__a" href="https://example.com/one">One</a>
            <a class="result__a" href="https://example.com/two">Two</a>
        "#;
        assert!(parse_ddg_results(html, 0).is_empty());
        assert!(parse_ddg_results("<p>No results</p>", 8).is_empty());
        let results = parse_ddg_results(html, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/one");
        assert_eq!(results[0].title, "One");
        assert!(results[0].snippet.is_empty());
        assert_eq!(parse_ddg_results(html, 8).len(), 2);
    }

    #[test]
    fn readable_html_removes_script_style_and_preserves_text_entities() {
        assert_eq!(
            html_to_text("<STYLE>hidden</STYLE><script>\nnot text\n</script><p>A&nbsp;&amp;&nbsp;B</p>\n\n\n<p>&lt;C&gt;</p>"),
            "A & B \n\n <C>"
        );
        assert_eq!(html_to_text("<script>hidden</script>"), "");
    }

    #[test]
    fn ranked_results_keep_their_own_snippets_when_limited() {
        let html = r#"
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone%3Fq%3Da%26b%3Dc&amp;rut=x">One &amp; <b>First</b></a>
            <a class="result__snippet">First &lt;summary&gt;</a>
            <a class="result__a" href="//example.com/two">Two</a>
            <a class="result__snippet">Second <b>summary</b></a>
        "#;
        for limit in [1, 2, 8] {
            let results = parse_ddg_results(html, limit);
            assert_eq!(results.len(), limit.min(2));
            assert_eq!(results[0].title, "One & First");
            assert_eq!(results[0].url, "https://example.com/one?q=a&b=c");
            assert_eq!(results[0].snippet, "First <summary>");
            if results.len() == 2 {
                assert_eq!(results[1].title, "Two");
                assert_eq!(results[1].url, "https://example.com/two");
                assert_eq!(results[1].snippet, "Second summary");
            }
        }
    }
}
