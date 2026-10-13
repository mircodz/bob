use crate::core::types::{ShellStream, ToolOutput};
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::SHELL_STREAM_TEXT_LIMIT;

const TEXT_OUTPUT_LIMIT: usize = 30_000;

pub(crate) fn bound_tool_output(content: ToolOutput, jobs: &JobRegistry) -> ToolOutput {
    match content {
        ToolOutput::Text(text) => bound_text_output(text, jobs).into(),
        ToolOutput::Read {
            body,
            loader_context,
        } => bound_read_output(body, loader_context, jobs),
        ToolOutput::Shell(mut shell) => {
            for stream in [&mut shell.stdout, &mut shell.stderr] {
                bound_shell_stream(stream, jobs);
            }
            ToolOutput::Shell(shell)
        }
    }
}

fn bound_read_output(
    body: String,
    mut loader_context: Option<String>,
    jobs: &JobRegistry,
) -> ToolOutput {
    const BODY_NOTICE_RESERVE: usize = 512;
    let attachment_bytes = loader_context
        .as_ref()
        .filter(|context| !context.is_empty())
        .map_or(0, |context| context.len() + 2);
    if body.len().saturating_add(attachment_bytes) <= TEXT_OUTPUT_LIMIT {
        return ToolOutput::Read {
            body,
            loader_context,
        };
    }
    if loader_context
        .as_ref()
        .is_some_and(|context| context.len() + 2 > TEXT_OUTPUT_LIMIT - BODY_NOTICE_RESERVE)
    {
        let context = loader_context.take().unwrap();
        let bytes = context.len();
        loader_context = Some(match jobs.retain_complete_output(context) {
            Some(id) => format!(
                "[!] Auxiliary project instructions omitted from this response ({bytes} bytes): the complete attachment does not fit the read-output budget. Use job_output with id \"{id}\" and offset/limit to read it. Retention is bounded and session-local; the attachment has not been applied in full."
            ),
            None => format!(
                "[!] Auxiliary project instructions omitted from this response ({bytes} bytes): the complete attachment exceeds the retention budget. Read the applicable instruction files directly; no partial instruction blocks are attached."
            ),
        });
    }
    let attachment_bytes = loader_context
        .as_ref()
        .filter(|context| !context.is_empty())
        .map_or(0, |context| context.len() + 2);
    let budget = TEXT_OUTPUT_LIMIT - attachment_bytes;
    let body = if body.len() > budget {
        let original_bytes = body.len();
        let id = jobs.retain_output(body.clone());
        let notice = format!(
            "\n\nRead body truncated ({original_bytes} bytes). Use job_output with id \"{id}\" and offset/limit to read retained body pages. Retention is bounded and session-local; pages report any further truncation."
        );
        let available = budget.saturating_sub(notice.len() + 80);
        let mut head = available * 5 / 7;
        let mut tail = body.len() - (available - head);
        while !body.is_char_boundary(head) {
            head -= 1;
        }
        while !body.is_char_boundary(tail) {
            tail += 1;
        }
        format!(
            "{}\n\n[{} body bytes omitted]\n\n{}{notice}",
            &body[..head],
            tail - head,
            &body[tail..],
        )
    } else {
        body
    };
    ToolOutput::Read {
        body,
        loader_context,
    }
}

fn bound_shell_stream(stream: &mut ShellStream, jobs: &JobRegistry) {
    if serde_json::to_string(&stream.text).unwrap().len() <= SHELL_STREAM_TEXT_LIMIT {
        return;
    }
    let original = std::mem::take(&mut stream.text);
    let mut budget = (SHELL_STREAM_TEXT_LIMIT - 200).min(original.len().saturating_sub(1));
    loop {
        let mut head = budget / 2;
        let mut tail = original.len() - budget / 2;
        while !original.is_char_boundary(head) {
            head -= 1;
        }
        while !original.is_char_boundary(tail) {
            tail += 1;
        }
        stream.text = format!(
            "{}\n... [{} bytes omitted] ...\n{}",
            &original[..head],
            tail - head,
            &original[tail..]
        );
        if serde_json::to_string(&stream.text).unwrap().len() <= SHELL_STREAM_TEXT_LIMIT {
            break;
        }
        budget /= 2;
    }
    stream.truncated = true;
    if stream.output_id.is_none() {
        stream.output_id = Some(jobs.retain_output(original));
    }
}

fn bound_text_output(content: String, jobs: &JobRegistry) -> String {
    const LIMIT: usize = TEXT_OUTPUT_LIMIT;
    if content.len() <= LIMIT {
        return content;
    }
    let mut head_end = 20_000;
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = content.len() - 8_000;
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let preview = format!(
        "{}\n\n[{} bytes omitted]\n\n{}",
        &content[..head_end],
        tail_start - head_end,
        &content[tail_start..]
    );
    let id = jobs.retain_output(content);
    format!("{preview}\n\nOutput truncated. Use job_output with id \"{id}\" and offset/limit to read retained pages. Retention is bounded and session-local; pages report any further truncation.")
}

#[cfg(test)]
mod tests {
    use crate::agent::output::bound_tool_output;
    use crate::tools::jobs::JobRegistry;

    fn recover(jobs: &JobRegistry, notice: &str) -> String {
        let id = notice
            .split("with id \"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let mut restored = String::new();
        let mut offset = 0;
        loop {
            let page = jobs.output_page(id, offset, 23_999).unwrap();
            assert!(!page.truncated);
            restored.push_str(&page.text);
            match page.next_offset {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => return restored,
            }
        }
    }

    #[test]
    fn read_body_budget_reserves_complete_context_and_retained_unicode_pages() {
        use crate::core::types::ToolOutput;
        let original = format!("first line\n{}\nlast line", "界🙂".repeat(20_000));
        for context in [
            None,
            Some("[!] Auxiliary instructions unavailable".into()),
            Some("界".repeat(9_800)),
        ] {
            let jobs = JobRegistry::new();
            let output = bound_tool_output(
                ToolOutput::Read {
                    body: original.clone(),
                    loader_context: context.clone(),
                },
                &jobs,
            );
            assert!(output.model_text().len() <= 30_000);
            let ToolOutput::Read {
                body,
                loader_context,
            } = &output
            else {
                panic!("read became untyped");
            };
            assert_eq!(loader_context, &context);
            assert!(body.starts_with("first line"));
            assert!(body.contains("last line"));
            assert_eq!(recover(&jobs, body), original);
            assert_eq!(bound_tool_output(output.clone(), &jobs), output);
        }
    }

    #[test]
    fn oversized_read_attachment_is_omitted_whole_and_retained_without_clipped_blocks() {
        use crate::core::project_context::{render_instructions, ProjectInstruction};
        use crate::core::types::ToolOutput;
        let context = render_instructions(
            &(0..4)
                .map(|index| ProjectInstruction {
                    path: format!("/repo/{index}/AGENTS.md").into(),
                    scope: format!("/repo/{index}").into(),
                    content: "\u{0001}".repeat(64 * 1024),
                })
                .collect::<Vec<_>>(),
        );
        assert!(context.len() > 1024 * 1024);
        let jobs = JobRegistry::new();
        let original_body = "界🙂\n".repeat(20_000);
        let output = bound_tool_output(
            ToolOutput::Read {
                body: original_body.clone(),
                loader_context: Some(context.clone()),
            },
            &jobs,
        );
        assert!(output.model_text().len() <= 30_000);
        let ToolOutput::Read {
            body,
            loader_context: Some(warning),
        } = &output
        else {
            panic!("expected bounded read with warning");
        };
        assert!(warning.starts_with("[!] Auxiliary project instructions omitted"));
        assert!(!warning.contains("<project-instruction>"));
        assert_eq!(recover(&jobs, warning), context);
        assert_eq!(recover(&jobs, body), original_body);
        assert_eq!(bound_tool_output(output.clone(), &jobs), output);
    }

    #[test]
    fn fitting_read_context_is_not_omitted_even_near_the_total_limit() {
        use crate::core::types::ToolOutput;
        let jobs = JobRegistry::new();
        let output = ToolOutput::Read {
            body: "界".into(),
            loader_context: Some("x".repeat(29_995)),
        };
        assert_eq!(output.model_text().len(), 30_000);
        assert_eq!(bound_tool_output(output.clone(), &jobs), output);
        assert!(jobs.output_page("output_1", 0, 100).is_none());
    }

    #[test]
    fn large_multibyte_output_is_bounded_with_retrievable_retained_pages() {
        let jobs = JobRegistry::new();
        let original = format!("failed: exit 7\n{}\nlast line", "🦀".repeat(20_000));
        let bounded = bound_tool_output(original.clone().into(), &jobs);
        let bounded = bounded.as_text().unwrap();
        assert!(bounded.len() <= 30_000);
        assert!(bounded.starts_with("failed: exit 7"));
        assert!(bounded.contains("last line"));
        assert!(bounded.contains("output_"));
        let id = bounded
            .split("with id \"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let mut restored = String::new();
        let mut offset = 0;
        loop {
            let page = jobs.output_page(id, offset, 1023).unwrap();
            assert_eq!(page.total_bytes, original.len());
            assert_eq!(page.offset, offset);
            assert!(!page.truncated);
            restored.push_str(&page.text);
            match page.next_offset {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(restored, original);
    }

    #[test]
    fn shell_bounds_preserve_status_and_retain_each_stream_before_json_escaping() {
        use crate::core::types::{ShellOutcome, ShellOutput, ShellStream, ToolOutput};
        for original in ["\0".repeat(3_000), "日本語".repeat(10_000)] {
            let jobs = JobRegistry::new();
            let stream = ShellStream {
                text: original.clone(),
                total_bytes: original.len() as u64,
                ..Default::default()
            };
            let output = bound_tool_output(
                ShellOutput {
                    signal: Some(9),
                    outcome: ShellOutcome::TimedOut,
                    stdout: stream.clone(),
                    stderr: stream,
                    ..Default::default()
                }
                .into(),
                &jobs,
            );
            let wire = output.model_text();
            assert!(wire.len() < 30_000);
            let parsed: ShellOutput = serde_json::from_str(&wire).unwrap();
            assert_eq!(parsed.signal, Some(9));
            assert_eq!(parsed.outcome, ShellOutcome::TimedOut);
            for stream in [&parsed.stdout, &parsed.stderr] {
                assert_eq!(stream.total_bytes, original.len() as u64);
                assert!(stream.truncated);
                assert!(stream.text.contains("bytes omitted"));
                let mut offset = 0;
                let mut recovered = String::new();
                loop {
                    let page = jobs
                        .output_page(stream.output_id.as_ref().unwrap(), offset, 1024)
                        .unwrap();
                    recovered.push_str(&page.text);
                    match page.next_offset {
                        Some(next) => offset = next,
                        None => break,
                    }
                }
                assert_eq!(recovered, original);
            }
            let ToolOutput::Shell(shell) = &output else {
                unreachable!()
            };
            assert_eq!(shell.as_ref(), &parsed);
            assert_eq!(bound_tool_output(output.clone(), &jobs), output);
        }
    }

    #[test]
    fn short_output_is_unchanged() {
        let jobs = JobRegistry::new();
        for output in [
            String::new(),
            "ok".into(),
            "x".repeat(30_000),
            "🦀".repeat(7500),
        ] {
            assert_eq!(
                bound_tool_output(output.clone().into(), &jobs).as_text(),
                Some(output.as_str())
            );
        }
        assert!(jobs.output_page("output_1", 0, 100).is_none());
        let bounded = bound_tool_output("x".repeat(30_001).into(), &jobs);
        let bounded = bounded.as_text().unwrap();
        assert!(bounded.contains("[2001 bytes omitted]"));
        assert!(bounded.contains("with id \"output_1\""));
    }
}
