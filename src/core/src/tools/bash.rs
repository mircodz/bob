//! The `bash` tool: run a shell command, with optional timeout and detached
//! (background-job) execution.

use crate::core::types::ToolSpec;
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
                Use a foreground timeout for bounded work. On timeout the shell is killed, but \
                descendant processes may outlive it. Set run_in_background:true for a service \
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

        // Background: spawn detached, register as a job, return its id at once. The
        // model collects the result later via job_status / job_output.
        if input["run_in_background"].as_bool().unwrap_or(false) {
            let id = ctx.jobs.next_id();
            let bg_cmd = command.clone();
            let work = async move {
                let out = tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(&bg_cmd)
                    .current_dir(&cwd)
                    .output()
                    .await;
                match out {
                    Ok(o) => (
                        if o.status.success() {
                            JobStatus::Done
                        } else {
                            JobStatus::Failed
                        },
                        combine_output(&o.stdout, &o.stderr, o.status.code()),
                    ),
                    Err(e) => (JobStatus::Failed, format!("failed to run: {}", e)),
                }
            };
            ctx.jobs
                .spawn(id.clone(), "bash", truncate_desc(&command), work);
            return Ok(format!(
                "started background job {id}: {}\nPoll with job_status / job_output.",
                truncate_desc(&command)
            ));
        }

        // Foreground: spawn the child so we hold a handle and can actually KILL it
        // on timeout. `kill_on_drop` means that when the timeout fires and the
        // `wait_with_output` future is dropped, tokio sends SIGKILL to the child
        // instead of leaving a detached process running.
        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::failed(e.to_string()))?;

        let collect = child.wait_with_output();
        let out = match input["timeout"].as_f64() {
            Some(secs) if secs > 0.0 => {
                let dur = std::time::Duration::from_secs_f64(secs);
                match tokio::time::timeout(dur, collect).await {
                    Ok(r) => r,
                    Err(_) => {
                        // Timed out: dropping `collect` (via early return) fires
                        // kill_on_drop and SIGKILLs the child.
                        return Err(ToolError::timeout(format!(
                            "command timed out after {}s and was killed; prefer \
                             run_in_background for long-running work",
                            secs
                        )));
                    }
                }
            }
            _ => collect.await,
        };

        match out {
            Ok(o) => Ok(cap_output(combine_output(
                &o.stdout,
                &o.stderr,
                o.status.code(),
            ))),
            Err(e) => Err(ToolError::failed(e.to_string())),
        }
    }
}

/// Merge stdout+stderr into the single text block the model sees, falling back to
/// an exit-code note when a command produced no output.
fn combine_output(stdout: &[u8], stderr: &[u8], code: Option<i32>) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let combined = [stdout.trim_end(), stderr.trim_end()]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let combined = combined.trim().to_string();
    if combined.is_empty() {
        format!("(exit {})", code.unwrap_or(-1))
    } else {
        combined
    }
}

/// Cap combined command output so a chatty build can't blow the context window.
/// Keeps the head and tail (where errors and summaries usually live) and notes how
/// much was elided in the middle.
fn cap_output(text: String) -> String {
    const LIMIT: usize = 30_000;
    if text.len() <= LIMIT {
        return text;
    }
    let head = 20_000;
    let tail = 8_000;
    let start: String = text.chars().take(head).collect();
    let end: String = text
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let elided = text.len().saturating_sub(head + tail);
    format!("{start}\n\n... [{elided} bytes truncated] ...\n\n{end}")
}

/// A short one-line description of a command, for the jobs panel.
fn truncate_desc(cmd: &str) -> String {
    let line = cmd.replace('\n', " ");
    if line.chars().count() > 60 {
        format!("{}...", line.chars().take(60).collect::<String>())
    } else {
        line
    }
}
