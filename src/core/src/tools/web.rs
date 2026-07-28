//! web_fetch: retrieve a URL and return readable text (HTML stripped).

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
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

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> String {
        let url = input["url"].as_str().unwrap_or("");
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return "error: url must start with http:// or https://".to_string();
        }
        let cap = input["max_chars"].as_u64().unwrap_or(20_000) as usize;

        let res = match reqwest::get(url).await {
            Ok(r) => r,
            Err(e) => return format!("error: {}", e),
        };
        if !res.status().is_success() {
            return format!("error: HTTP {} fetching {}", res.status().as_u16(), url);
        }
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match res.text().await {
            Ok(b) => b,
            Err(e) => return format!("error: {}", e),
        };
        let text = if content_type.contains("html") {
            html_to_text(&body)
        } else {
            body
        };
        if text.chars().count() > cap {
            let truncated: String = text.chars().take(cap).collect();
            format!("{}\n…[truncated]", truncated)
        } else {
            text
        }
    }
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
