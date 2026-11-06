use anyhow::Result;
use bob_core::tools::web::{WebClient, WebRequest, MAX_WEB_PREVIEW_CHARS};
use bob_kernel::{EffectClass, ToolDescriptor, ToolOutcome, ToolResultPresentation};
use bob_types::ToolSpec;
use serde_json::json;

pub(super) fn descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            effect: EffectClass::Read,
            spec: ToolSpec {
                name: "web_fetch".into(),
                description: "Fetch a specific HTTP(S) URL and return readable text (HTML is stripped). This does not search the web. max_chars bounds the preview; full text is retained. Responses larger than 4 MiB are rejected; requests time out after 30 seconds.".into(),
                input_schema: json!({"type":"object","additionalProperties":false,"required":["url"],"properties":{
                    "url":{"type":"string"},
                    "max_chars":{"type":"integer","minimum":1,"maximum":MAX_WEB_PREVIEW_CHARS,"description":"Preview character limit (default 20000)."}
                }}),
            },
        },
        ToolDescriptor {
            effect: EffectClass::Read,
            spec: ToolSpec {
                name: "web_search".into(),
                description: "Search the web and return ranked results with titles, URLs, and short snippets. Use for current information or when no URL is known, then web_fetch relevant results. Returns snippets, not full pages.".into(),
                input_schema: json!({"type":"object","additionalProperties":false,"required":["query"],"properties":{
                    "query":{"type":"string"},
                    "max_results":{"type":"integer","minimum":1,"maximum":20,"description":"Result limit (default 8)."}
                }}),
            },
        },
    ]
}

pub(super) async fn execute(client: &WebClient, request: &WebRequest) -> Result<ToolOutcome> {
    let response = client.execute(request).await?;
    Ok(ToolOutcome {
        text: response.text,
        is_error: false,
        interrupted: false,
        presentation: Some(ToolResultPresentation {
            summary: Some(
                match request {
                    WebRequest::Fetch { .. } => "Fetched page",
                    WebRequest::Search { .. } => "Search results",
                }
                .into(),
            ),
            body_preview: Some(response.preview),
            exit_code: None,
        }),
    })
}
