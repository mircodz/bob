//! The `bash` tool: run a shell command, with optional timeout and detached
//! (background-job) execution.

use crate::core::types::ToolSpec;
use crate::tools::jobs::JobStatus;
use crate::tools::registry::{Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description:
                "Run a shell command via `bash -c` and return its combined stdout/stderr. \
                Use this to actually DO things: run builds, tests, linters, git, package managers, \
                and scripts. Do NOT use it to read, search, or list files — use read_file, grep, \
                glob, and list_dir instead (they're faster and cleaner). Guidance: commands run \
                from the working directory, so don't `cd` unless asked; quote paths that contain \
                spaces; chain related steps with `&&`; avoid destructive commands (`rm -rf`, \
                `git push`, `git reset --hard`) unless explicitly requested; never commit or push \
                unless the user asks. Set `timeout` (seconds) to bound a command that might hang; \
                it's killed and reported if it exceeds that. Set `run_in_background: true` for \
                long-running work (a dev server, a watch build) you don't want to block on — it \
                returns a job id immediately; poll it with job_status / job_output."
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let command = input["command"].as_str().unwrap_or("").to_string();
        let cwd = ctx.cwd.clone();

        // Background: spawn detached, register as a job, return its id at once. The
        // model collects the result later via job_status / job_output.
        if input["run_in_background"].as_bool().unwrap_or(false) {
            let id = ctx.jobs.next_id();
            let jobs = ctx.jobs.clone();
            let job_id = id.clone();
            let bg_cmd = command.clone();
            let handle = tokio::spawn(async move {
                let out = tokio::task::spawn_blocking(move || {
                    std::process::Command::new("bash")
                        .arg("-c")
                        .arg(&bg_cmd)
                        .current_dir(&cwd)
                        .output()
                })
                .await;
                let (status, text) = match out {
                    Ok(Ok(o)) => (
                        if o.status.success() {
                            JobStatus::Done
                        } else {
                            JobStatus::Failed
                        },
                        combine_output(&o.stdout, &o.stderr, o.status.code()),
                    ),
                    Ok(Err(e)) => (JobStatus::Failed, format!("error: {}", e)),
                    Err(e) => (JobStatus::Failed, format!("error: {}", e)),
                };
                jobs.finish(&job_id, status, text);
            });
            ctx.jobs
                .register(id.clone(), "bash", truncate_desc(&command), handle);
            return format!(
                "started background job {id}: {}\nPoll with job_status / job_output.",
                truncate_desc(&command)
            );
        }

        // Foreground: run to completion, optionally bounded by a timeout.
        let run = tokio::task::spawn_blocking(move || {
            std::process::Command::new("bash")
                .arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .output()
        });

        let result = match input["timeout"].as_f64() {
            Some(secs) if secs > 0.0 => {
                let dur = std::time::Duration::from_secs_f64(secs);
                match tokio::time::timeout(dur, run).await {
                    Ok(r) => r,
                    Err(_) => {
                        return format!(
                            "error: command timed out after {}s (still running detached; \
                             it was not killed cleanly — prefer run_in_background for long work)",
                            secs
                        );
                    }
                }
            }
            _ => run.await,
        };

        match result {
            Ok(Ok(out)) => combine_output(&out.stdout, &out.stderr, out.status.code()),
            Ok(Err(e)) => format!("error: {}", e),
            Err(e) => format!("error: {}", e),
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

/// A short one-line description of a command, for the jobs panel.
fn truncate_desc(cmd: &str) -> String {
    let line = cmd.replace('\n', " ");
    if line.chars().count() > 60 {
        format!("{}...", line.chars().take(60).collect::<String>())
    } else {
        line
    }
}
