use super::{arg, preview, text, work_style, WorkResult};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;

pub(super) const EXPANDED_ROWS: usize = 24;
const ITEMS: usize = 8;

pub(super) fn handles(name: &str) -> bool {
    matches!(name, "search_conversation" | "read_conversation")
}

pub(super) fn detail(input: &Value) -> String {
    [
        input["view"].as_str().unwrap_or("transcript"),
        arg(input, "session_id"),
        arg(input, "thread_id"),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .map(|part| preview(part, 48))
    .collect::<Vec<_>>()
    .join(" · ")
}

fn line(result: &mut WorkResult, value: &str, style: Style) {
    result
        .lines
        .push(Line::from(Span::styled(preview(value, 320), style)));
}

fn more(value: &Value) -> bool {
    !value["next_cursor"].is_null() || !value["next_offset"].is_null()
}

fn warnings(result: &mut WorkResult, value: &Value) {
    let mut notes = Vec::new();
    if let Some(limitations) = value["limitations"].as_array() {
        for limitation in limitations.iter().take(ITEMS) {
            let label = match arg(limitation, "code") {
                "source_missing" => "missing sources",
                "source_corrupt" => "unreadable sources",
                "source_oversized" => "oversized sources skipped",
                "preview_only" => "preview-only sources",
                "metadata_truncated" => "metadata shortened",
                "preceding_context_limited" => "context windows limited by scan budget",
                other => other,
            };
            let count = limitation["count"].as_u64().unwrap_or(0);
            notes.push(format!("{count} {label}"));
        }
        if limitations.len() > ITEMS {
            notes.push("additional limitations in full output".into());
        }
    }
    if !notes.is_empty() {
        line(
            result,
            &format!("Coverage: {}", notes.join(" · ")),
            work_style("unknown"),
        );
    }
    let entries = value["entries"].as_array();
    if value["preview_only"] == true
        || value["retention"] == "preview_only"
        || entries.is_some_and(|entries| entries.iter().any(|entry| entry["preview_only"] == true))
    {
        line(
            result,
            "Only a source preview is retained for some content",
            work_style("unknown"),
        );
    }
    if value["text_truncated"] == true
        || entries
            .is_some_and(|entries| entries.iter().any(|entry| entry["text_truncated"] == true))
    {
        line(
            result,
            "Entry excerpts shortened · read entry for retained text",
            work_style("unknown"),
        );
    }
    if value["page_truncated"] == true && !more(value) {
        line(
            result,
            "Returned text page is partial",
            work_style("unknown"),
        );
    }
}

fn items(result: &mut WorkResult, values: &[Value], search: bool, threads: bool) {
    for value in values.iter().take(ITEMS) {
        if threads {
            line(
                result,
                &format!("{} · {}", arg(value, "thread_id"), arg(value, "name")),
                text(),
            );
            continue;
        }
        let mut label = Vec::new();
        if search {
            for key in ["title", "workspace"] {
                let part = arg(value, key);
                if !part.is_empty() {
                    label.push(preview(part, 56));
                }
            }
        }
        if let Some(sequence) = value["sequence"].as_i64() {
            label.push(format!("#{sequence}"));
        }
        if !arg(value, "kind").is_empty() {
            label.push(arg(value, "kind").into());
        }
        if value["preview_only"] == true {
            label.push("source preview only".into());
        }
        if !label.is_empty() {
            line(result, &label.join(" · "), text());
        }
        line(
            result,
            arg(value, if search { "snippet" } else { "text" }),
            text(),
        );
    }
    if values.len() > ITEMS {
        line(result, "Additional results in full output", text());
    }
}

pub(super) fn result(name: &str, source: &str, failed: bool) -> Option<WorkResult> {
    if !handles(name) {
        return None;
    }
    let mut result = WorkResult {
        lines: Vec::new(),
        failed,
        structured: true,
    };
    if failed {
        line(&mut result, source, work_style("failed"));
        return Some(result);
    }
    let Ok(value) = serde_json::from_str::<Value>(source) else {
        line(
            &mut result,
            "Retained conversation page · inspect full output",
            text(),
        );
        return Some(result);
    };
    let coverage = match value["scan_complete"].as_bool() {
        Some(true) => "scan complete",
        Some(false) => "partial scan",
        None => "coverage not reported",
    };
    let pagination = if more(&value) {
        " · more pages available"
    } else {
        ""
    };
    if name == "search_conversation" {
        let Some(matches) = value["matches"].as_array() else {
            line(
                &mut result,
                "Retained search page · inspect full output",
                text(),
            );
            return Some(result);
        };
        let summary = if matches.is_empty() {
            if value["scan_complete"] == true {
                "No matches in available content".into()
            } else {
                "No matches in examined content".into()
            }
        } else {
            format!("{} matches on this page", matches.len())
        };
        line(
            &mut result,
            &format!("{summary} · {coverage}{pagination}"),
            text(),
        );
        warnings(&mut result, &value);
        if let (Some(entries), Some(bytes)) = (
            value["examined_entries"].as_u64(),
            value["examined_bytes"].as_u64(),
        ) {
            line(
                &mut result,
                &format!("Examined {entries} entries · {bytes} bytes"),
                text(),
            );
        }
        items(&mut result, matches, true, false);
    } else {
        let entries = value["entries"].as_array();
        let threads = value["threads"].as_array();
        let summary = if let Some(entries) = entries {
            format!("{} conversation entries · {coverage}", entries.len())
        } else if let Some(threads) = threads {
            format!("{} threads · {coverage}", threads.len())
        } else {
            "Retained text page".into()
        };
        line(&mut result, &format!("{summary}{pagination}"), text());
        warnings(&mut result, &value);
        if let Some(entries) = entries {
            items(&mut result, entries, false, false);
        } else if let Some(threads) = threads {
            items(&mut result, threads, false, true);
        } else {
            line(&mut result, arg(&value, "text"), text());
        }
    }
    Some(result)
}
