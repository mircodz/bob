//! Chooses how the full-output window colors a retained tool result: syntax
//! highlighting for files and code, diff colors for edits, Markdown for agent
//! reports and pretty-printed JSON for anything structured.

use super::style::{text, Palette};
use bob_ui::{diffview, highlight::highlight, markdown::render_markdown};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;

/// Pages larger than this render plain so opening the window stays instant.
const STYLED_BYTES: usize = 64 * 1024;

pub(super) enum Kind {
    Plain,
    Code(String),
    Markdown,
}

pub(super) fn kind(tool: &str, input: &Value, page: &str) -> Kind {
    let path = input["path"].as_str().unwrap_or("");
    match tool {
        "read_file" => Kind::Code(extension(path)),
        "explore" | "task" | "web_fetch" | "spawn_agent" | "launch_agent" => Kind::Markdown,
        "bash" if page.contains("\ndiff --git ") || page.starts_with("diff --git ") => {
            Kind::Code("diff".into())
        }
        _ if looks_like_json(page) => Kind::Code("json".into()),
        _ => Kind::Plain,
    }
}

pub(super) fn lines(kind: &Kind, page: &str) -> Vec<Line<'static>> {
    if page.len() > STYLED_BYTES {
        return plain(page);
    }
    match kind {
        Kind::Plain => plain(page),
        Kind::Markdown => render_markdown(page),
        Kind::Code(language) if language == "diff" => page
            .split('\n')
            .map(|line| match line.chars().next() {
                Some('+') if !line.starts_with("+++") => {
                    Line::styled(line.to_owned(), Style::default().fg(Palette::DIFF_ADD()))
                }
                Some('-') if !line.starts_with("---") => {
                    Line::styled(line.to_owned(), Style::default().fg(Palette::DIFF_REMOVE()))
                }
                Some('@') => Line::styled(line.to_owned(), Style::default().fg(Palette::ACCENT())),
                _ if line.starts_with("diff --git") => diffview::diff_header(line),
                _ => Line::styled(line.to_owned(), text()),
            })
            .collect(),
        Kind::Code(language) if language == "json" => {
            let pretty = serde_json::from_str::<Value>(page)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok());
            highlight(pretty.as_deref().unwrap_or(page), "json")
        }
        Kind::Code(language) => numbered(page, language),
    }
}

/// `read_file` pages carry a right-aligned line number and a tab before each
/// line; highlight the code and keep the numbers as a dim gutter.
fn numbered(page: &str, language: &str) -> Vec<Line<'static>> {
    let mut gutters = Vec::new();
    let mut code = Vec::new();
    let mut trailer = Vec::new();
    for line in page.split('\n') {
        match line.split_once('\t') {
            Some((number, source))
                if trailer.is_empty()
                    && !number.trim().is_empty()
                    && number.trim().chars().all(|c| c.is_ascii_digit()) =>
            {
                gutters.push(number.to_owned());
                code.push(source);
            }
            _ if code.is_empty() => return highlight(page, language),
            _ => trailer.push(line),
        }
    }
    let mut out: Vec<Line<'static>> = gutters
        .into_iter()
        .zip(highlight(&code.join("\n"), language))
        .map(|(number, line)| {
            let mut spans = vec![Span::styled(
                format!("{number}  "),
                Style::default().fg(Palette::FAINT()),
            )];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();
    out.extend(
        trailer
            .into_iter()
            .map(|line| Line::styled(line.to_owned(), Style::default().fg(Palette::DIM()))),
    );
    out
}

fn plain(page: &str) -> Vec<Line<'static>> {
    page.split('\n')
        .map(|line| Line::styled(line.to_owned(), text()))
        .collect()
}

fn extension(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_owned()
}

fn looks_like_json(page: &str) -> bool {
    page.trim_start().starts_with(['{', '['])
        && serde_json::from_str::<Value>(page)
            .is_ok_and(|value| value.is_object() || value.is_array())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shown(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn read_file_pages_keep_numbers_and_highlight_code() {
        let page = "     1\tfn main() {\n     2\t    println!(\"hi\");\n     3\t}\n\n... [output truncated; use offset=4 and limit to read further]";
        let kind = kind("read_file", &json!({"path":"src/main.rs"}), page);
        let lines = lines(&kind, page);
        assert_eq!(lines.len(), 5);
        assert!(shown(&lines).starts_with("     1  fn main() {"));
        assert!(
            lines[1].spans.len() > 2,
            "code is highlighted: {:?}",
            lines[1]
        );
        assert!(shown(&lines).ends_with("limit to read further]"));
    }

    #[test]
    fn agent_reports_render_as_markdown_and_json_is_pretty_printed() {
        let report = "## explorer · completed\n\n**Short answer:** it works.";
        let rendered = lines(&kind("explore", &json!({}), report), report);
        assert!(!shown(&rendered).contains("**"));
        let page = r#"{"status":"ok","items":[1,2]}"#;
        let rendered = lines(&kind("web_search", &json!({}), page), page);
        assert!(rendered.len() > 3);
        assert!(shown(&rendered).contains("\"status\": \"ok\""));
    }

    #[test]
    fn shell_output_stays_plain_unless_it_is_a_diff() {
        let page = "Compiling bob\nFinished";
        assert!(matches!(kind("bash", &json!({}), page), Kind::Plain));
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new";
        let lines = lines(&kind("bash", &json!({}), diff), diff);
        assert_eq!(lines[4].style.fg, Some(Palette::DIFF_REMOVE()));
        assert_eq!(lines[5].style.fg, Some(Palette::DIFF_ADD()));
    }
}
