use crate::core::process::{ManagedProcess, ProcessOutput};
use crate::core::types::{ShellOutcome, ShellOutput, ShellStream, ToolSpec};
use crate::tools::jobs::JobStatus;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description:
                "Run a shell command with bash -c for builds, tests, git, package managers, \
                or scripts. Prefer dedicated read/search tools for inspecting files. Commands \
                start in the working directory; quote paths and keep commands non-interactive. \
                Use a foreground timeout for bounded work. Timeout and cancellation terminate \
                the owned process group (Unix) or job (Windows) and reap the shell. Processes \
                that deliberately leave that group may escape. Set run_in_background:true for a service \
                or work you can collect later with job_output; timeout does not apply to \
                background jobs. Do not repeatedly poll or run sleeps just to wait. \
                Foreground output keeps the head and tail when it exceeds roughly 30k bytes. \
                For verification, check an explicit exit status or test result rather than \
                assuming returned text means success. Never commit, push, or run destructive \
                actions without the user's authorization."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "number", "description": "Max seconds to wait before killing the command (foreground only)." },
                    "run_in_background": { "type": "boolean", "description": "Run detached and return a job id instead of blocking." }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let command = input["command"].as_str().unwrap_or("").to_string();
        let cwd = ctx.cwd.clone();
        if input["run_in_background"].as_bool().unwrap_or(false) {
            let id = ctx.jobs.next_id();
            let bg_cmd = command.clone();
            let accepted = ctx.jobs.spawn_cancellable(
                id.clone(),
                "bash",
                truncate_desc(&command),
                move |mut cancel| async move {
                    if *cancel.borrow() {
                        return (JobStatus::Cancelled, ShellOutput {
                            outcome: ShellOutcome::Cancelled,
                            ..ShellOutput::default()
                        }.into());
                    }
                    let mut process = match spawn_shell(&bg_cmd, &cwd) {
                        Ok(process) => process,
                        Err(error) => {
                            return (JobStatus::Failed, format!("failed to run: {error}").into())
                        }
                    };
                    let result = tokio::select! {
                        biased;
                        _ = cancel.changed() => {
                            let result = process.terminate_and_collect(100_000).await;
                            return (JobStatus::Cancelled, match result {
                                Ok(output) => shell_output(output, ShellOutcome::Cancelled).into(),
                                Err(error) => format!("cancellation cleanup failed: {error}").into(),
                            });
                        }
                        result = process.collect_output(100_000) => result,
                    };
                    match result {
                        Ok(output) => (
                            if output.status.success() {
                                JobStatus::Done
                            } else {
                                JobStatus::Failed
                            },
                            shell_output(output, ShellOutcome::Exited).into(),
                        ),
                        Err(error) => {
                            let _ = process.terminate_and_wait().await;
                            (JobStatus::Failed, format!("failed to run: {error}").into())
                        }
                    }
                },
            );
            if !accepted {
                return Err(ToolError::failed(
                    "background jobs are shutting down or at capacity",
                ));
            }
            return Ok(format!(
                "started background job {id}: {}\nCollect with job_output; use offset/limit for more retained output.",
                truncate_desc(&command)
            ).into());
        }

        let timeout = match input["timeout"].as_f64() {
            Some(seconds) if seconds > 0.0 => Some(
                std::time::Duration::try_from_secs_f64(seconds)
                    .map_err(|_| ToolError::invalid_input("timeout must be a finite duration"))?,
            ),
            _ => None,
        };
        let mut process =
            spawn_shell(&command, &cwd).map_err(|error| ToolError::failed(error.to_string()))?;
        let output = if let Some(duration) = timeout {
            match tokio::time::timeout(duration, process.collect_output(14_000)).await {
                Ok(result) => result,
                Err(_) => {
                    let output = process
                        .terminate_and_collect(14_000)
                        .await
                        .map_err(|error| {
                            ToolError::failed(format!("timeout cleanup failed: {error}"))
                        })?;
                    return Err(ToolError::timeout(format!(
                        "command timed out after {}s; owned process tree terminated",
                        duration.as_secs_f64(),
                    ))
                    .with_output(shell_output(output, ShellOutcome::TimedOut)));
                }
            }
        } else {
            process.collect_output(14_000).await
        };
        match output {
            Ok(output) if output.status.success() => {
                Ok(shell_output(output, ShellOutcome::Exited).into())
            }
            Ok(output) => Err(ToolError::failed("command failed")
                .with_output(shell_output(output, ShellOutcome::Exited))),
            Err(error) => {
                let _ = process.terminate_and_wait().await;
                Err(ToolError::failed(error.to_string()))
            }
        }
    }
}

fn spawn_shell(command: &str, cwd: &str) -> std::io::Result<ManagedProcess> {
    ManagedProcess::spawn(
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
    )
}

fn shell_output(output: ProcessOutput, outcome: ShellOutcome) -> ShellOutput {
    #[cfg(unix)]
    let (signal, core_dumped) = {
        use std::os::unix::process::ExitStatusExt;
        (output.status.signal(), output.status.core_dumped())
    };
    #[cfg(not(unix))]
    let (signal, core_dumped) = (None, false);
    let stream = |capture: crate::core::process::CapturedOutput| ShellStream {
        text: capture.text(),
        total_bytes: capture.total_bytes,
        truncated: capture.truncated(),
        output_id: None,
    };
    ShellOutput {
        exit_code: output.status.code(),
        signal,
        core_dumped,
        outcome,
        stdout: stream(output.stdout),
        stderr: stream(output.stderr),
        page: None,
    }
}

fn truncate_desc(cmd: &str) -> String {
    let line = cmd.replace('\n', " ");
    if line.chars().count() > 60 {
        format!("{}...", line.chars().take(60).collect::<String>())
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ToolOutput;
    use std::sync::Arc;

    fn shell(output: ToolOutput) -> ShellOutput {
        match output {
            ToolOutput::Shell(output) => *output,
            ToolOutput::Text(text) => panic!("expected shell output, got {text}"),
        }
    }

    fn context() -> ToolContext {
        ToolContext {
            cwd: ".".into(),
            files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
            todos: Arc::new(crate::tools::todo::TodoStore::new()),
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            coord: None,
            permissions: None,
        }
    }

    #[test]
    fn command_descriptions_count_characters_not_utf8_bytes() {
        for input in ["", "echo hello", "first\nsecond", &"界".repeat(60)] {
            assert_eq!(truncate_desc(input), input.replace('\n', " "));
        }
        assert_eq!(
            truncate_desc(&"🙂".repeat(61)),
            format!("{}...", "🙂".repeat(60))
        );
        assert_eq!(
            truncate_desc(&format!("{}\nend", "x".repeat(59))),
            format!("{} ...", "x".repeat(59))
        );
    }

    #[cfg(unix)]
    #[test]
    fn structured_output_preserves_raw_counts_and_lossy_text() {
        use crate::core::process::CapturedOutput;
        use std::os::unix::process::ExitStatusExt;
        let output = ProcessOutput {
            status: std::process::ExitStatus::from_raw(7 << 8),
            stdout: CapturedOutput {
                bytes: b"out\n".to_vec(),
                total_bytes: 4,
                incomplete: false,
            },
            stderr: CapturedOutput {
                bytes: b"a\xffxy".to_vec(),
                total_bytes: 9,
                incomplete: false,
            },
        };
        let output = shell_output(output, ShellOutcome::Exited);
        assert_eq!(output.exit_code, Some(7));
        assert_eq!(output.stdout.text, "out\n");
        assert_eq!(output.stdout.total_bytes, 4);
        assert!(!output.stdout.truncated);
        assert_eq!(
            output.stderr.text,
            "a\u{fffd}\n... [5 bytes truncated] ...\nxy"
        );
        assert_eq!(output.stderr.total_bytes, 9);
        assert!(output.stderr.truncated);
    }

    #[tokio::test]
    async fn unrepresentable_timeout_is_rejected_before_shell_launch() {
        let ctx = context();
        let error = BashTool
            .execute(json!({"command":"unused", "timeout":1e100}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            crate::tools::registry::ToolErrorKind::InvalidInput
        );
        assert_eq!(error.message, "timeout must be a finite duration");
        assert!(ctx.jobs.list().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identical_nonempty_output_preserves_success_and_failure() {
        let ctx = context();
        let success = BashTool
            .execute(json!({"command": "printf identical; exit 0"}), &ctx)
            .await
            .unwrap();
        let failure = BashTool
            .execute(json!({"command": "printf identical; exit 7"}), &ctx)
            .await
            .unwrap_err();
        let success = shell(success);
        let failure = shell(failure.into_output());
        assert_eq!(success.exit_code, Some(0));
        assert_eq!(failure.exit_code, Some(7));
        assert_eq!(success.stdout.text, "identical");
        assert_eq!(failure.stdout, success.stdout);
        assert_eq!(success.outcome, ShellOutcome::Exited);
        assert_eq!(failure.outcome, ShellOutcome::Exited);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_termination_is_not_an_unknown_exit_code() {
        let error = BashTool
            .execute(
                json!({"command": "printf signal; kill -TERM $$"}),
                &context(),
            )
            .await
            .unwrap_err();
        let output = shell(error.into_output());
        assert_eq!(output.exit_code, None);
        assert_eq!(output.signal, Some(15));
        assert!(!output.core_dumped);
        assert_eq!(output.stdout.text, "signal");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_is_terminal_and_typed() {
        let error = BashTool
            .execute(
                json!({"command": "printf before; printf error >&2; sleep 60", "timeout": 0.2}),
                &context(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, crate::tools::registry::ToolErrorKind::Timeout);
        assert!(error.message.contains("owned process tree terminated"));
        let output = shell(error.into_output());
        assert_eq!(output.outcome, ShellOutcome::TimedOut);
        assert_eq!(output.signal, Some(9));
        assert_eq!(output.stdout.text, "before");
        assert_eq!(output.stdout.total_bytes, 6);
        assert_eq!(output.stderr.text, "error");
        assert_eq!(output.stderr.total_bytes, 5);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_output_is_bounded_and_pageable() {
        let ctx = context();
        BashTool.execute(json!({"command": "printf '%300000s' x; printf tail; printf 'stderr終' >&2; exit 9", "run_in_background": true}), &ctx).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while ctx.jobs.status_of("job_1") == Some(JobStatus::Running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(ctx.jobs.status_of("job_1"), Some(JobStatus::Failed));
        let mut offset = 0;
        let mut stdout = String::new();
        let mut stderr = String::new();
        let gap = "\n... [200004 bytes truncated] ...\n";
        loop {
            let output = crate::tools::jobs::JobOutputTool
                .execute(
                    json!({"id": "job_1", "offset": offset, "limit": 1000}),
                    &ctx,
                )
                .await
                .unwrap();
            let output = shell(output);
            assert_eq!(output.exit_code, Some(9));
            assert_eq!(output.outcome, ShellOutcome::Exited);
            assert_eq!(output.stdout.total_bytes, 300_004);
            assert_eq!(output.stderr.total_bytes, 9);
            assert!(output.stdout.truncated);
            assert!(!output.stderr.truncated);
            assert!(output.stdout.text.len() + output.stderr.text.len() <= 1000);
            stdout.push_str(&output.stdout.text);
            stderr.push_str(&output.stderr.text);
            let page = output.page.unwrap();
            assert_eq!(page.offset, offset);
            assert_eq!(page.retained_bytes, 100_009 + gap.len());
            match page.next_offset {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(stdout.len(), 100_000 + gap.len());
        assert!(stdout.contains(gap));
        assert!(stdout.ends_with("xtail"));
        assert_eq!(stderr, "stderr終");
        assert!(ctx.jobs.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_cancellation_retains_both_streams() {
        let ctx = context();
        let (ready, started) = tokio::sync::oneshot::channel();
        ctx.jobs.spawn_cancellable(
            "cancelled".into(),
            "bash",
            "cancel".into(),
            move |mut cancel| async move {
                let mut process =
                    spawn_shell("printf before; printf error >&2; sleep 60", ".").unwrap();
                let mut collection = Box::pin(process.collect_output(100_000));
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    &mut collection
                )
                .await
                .is_err());
                drop(collection);
                ready.send(()).unwrap();
                cancel.changed().await.unwrap();
                let output = process.terminate_and_collect(100_000).await.unwrap();
                (
                    JobStatus::Cancelled,
                    shell_output(output, ShellOutcome::Cancelled).into(),
                )
            },
        );
        started.await.unwrap();
        assert!(ctx.jobs.cancel_and_wait("cancelled").await);
        let page = ctx.jobs.output_page("cancelled", 0, 1000).unwrap();
        assert_eq!(page.status, JobStatus::Cancelled);
        let output = page.shell.unwrap();
        assert_eq!(output.outcome, ShellOutcome::Cancelled);
        assert_eq!(output.signal, Some(9));
        assert_eq!(output.stdout.text, "before");
        assert_eq!(output.stdout.total_bytes, 6);
        assert_eq!(output.stderr.text, "error");
        assert_eq!(output.stderr.total_bytes, 5);
        assert!(ctx.jobs.shutdown().await);
    }

    #[tokio::test]
    async fn closed_registry_rejects_background_launch() {
        let ctx = context();
        assert!(ctx.jobs.shutdown().await);
        assert!(BashTool
            .execute(
                json!({"command": "unused", "run_in_background": true}),
                &ctx
            )
            .await
            .is_err());
        assert!(ctx.jobs.list().is_empty());
    }
}
