//! User-interaction tools: `ask_user` (pose a question with options) and
//! `exit_plan` (present a plan for approval). Both delegate to the UI via the
//! `UserAsker` hook on the ToolContext.

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult, UserQuery};
use async_trait::async_trait;
use serde_json::{json, Value};

/// `ask_user`: pose a question to the user with a few options. Use when a
/// decision is genuinely the user's to make and you can't resolve it yourself.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".to_string(),
            description: "Ask the user a question when a choice is genuinely theirs to make (a \
                preference, an ambiguous requirement, which approach to take) and you can't \
                resolve it from the code or sensible defaults. Provide 2-4 concrete options; the \
                user can always pick 'Other' and type their own answer. Prefer just doing the \
                work when the answer is obvious — don't over-ask."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to ask." },
                    "detail": { "type": "string", "description": "Optional extra context (Markdown)." },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "2-4 concrete answer options."
                    }
                },
                "required": ["question", "options"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let question = input["question"].as_str().unwrap_or("").to_string();
        let detail = input["detail"].as_str().unwrap_or("").to_string();
        let options: Vec<String> = input["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let query = UserQuery {
            title: question,
            detail,
            options,
            allow_other: true,
        };
        match &ctx.user_asker {
            Some(asker) => match asker.ask(&query).await {
                Some(answer) => Ok(format!("user answered: {}", answer)),
                None => Ok("user dismissed the question without answering".to_string()),
            },
            None => Err(ToolError::unavailable(
                "no interactive UI available to ask the user",
            )),
        }
    }
}

/// `exit_plan`: save the plan bob has formed as a markdown document under
/// `~/.bob/plans/`, then ask the user to approve it (or request changes). Called
/// at the end of plan mode. Returns the user's decision plus the saved path so
/// bob knows whether to proceed and can reference/refine the artifact.
pub struct ExitPlanTool;

/// Turn a plan into a stable, filesystem-safe file stem: a slug of its first
/// markdown heading (or "plan"), plus a short content hash so distinct plans
/// never collide. Deterministic — no wall-clock or RNG (core avoids both).
fn plan_slug(plan: &str) -> String {
    use sha2::{Digest, Sha256};
    // First `# heading` line, else the first non-empty line, else "plan".
    let title = plan
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim())
        .or_else(|| plan.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("plan");
    let mut slug = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_') && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "plan" } else { slug };
    let slug: String = slug.chars().take(48).collect();
    let slug = slug.trim_end_matches('-');

    let digest = Sha256::digest(plan.as_bytes());
    let suffix: String = digest
        .iter()
        .take(4)
        .map(|b| format!("{:02x}", b))
        .collect();
    format!("{slug}-{suffix}")
}

/// Write the plan to `~/.bob/plans/<slug>.md`, returning the path on success.
/// Best-effort: a failure returns None so the approval flow still proceeds.
fn save_plan(plan: &str) -> Option<std::path::PathBuf> {
    let dir = crate::core::config::plans_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.md", plan_slug(plan)));
    std::fs::write(&path, plan).ok()?;
    Some(path)
}

#[async_trait]
impl Tool for ExitPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_plan".to_string(),
            description: "Present your implementation plan and ask the user to approve it before \
                you start making changes. Call this ONLY in plan mode, once you've researched \
                enough to propose concrete steps. Pass the plan as Markdown — it is SAVED as a \
                document under ~/.bob/plans/ and shown to the user, who either approves (you may \
                then proceed and edits are unblocked) or asks you to refine it (call this again \
                with the revised plan)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "string", "description": "The proposed plan, as Markdown." }
                },
                "required": ["plan"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let plan = input["plan"].as_str().unwrap_or("").to_string();
        // Persist the plan as a document first (best-effort).
        let saved = save_plan(&plan);
        let query = UserQuery {
            title: "Ready to code?".to_string(),
            detail: plan,
            options: vec![
                "Yes, proceed".to_string(),
                "No, keep refining the plan".to_string(),
            ],
            allow_other: true,
        };
        match &ctx.user_asker {
            Some(asker) => match asker.ask(&query).await {
                Some(answer) => {
                    // The UI approves by switching the mode out of Plan; here we
                    // relay the user's words back to the model, plus where the plan
                    // was saved so the model can reference or refine the artifact.
                    match saved {
                        Some(path) => Ok(format!(
                            "user responded: {}\n(plan saved to {})",
                            answer,
                            path.display()
                        )),
                        None => Ok(format!("user responded: {}", answer)),
                    }
                }
                None => Ok("user dismissed the plan approval".to_string()),
            },
            None => Err(ToolError::unavailable(
                "no interactive UI available to present the plan",
            )),
        }
    }
}

/// `enter_plan`: the agent puts ITSELF into read-only plan mode. Use when a task
/// is large, risky, or ambiguous enough to warrant researching and proposing a
/// plan before touching anything. Switching to Plan mode blocks all mutating tools
/// until the plan is approved via `exit_plan`.
pub struct EnterPlanTool;

#[async_trait]
impl Tool for EnterPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "enter_plan".to_string(),
            description:
                "Switch yourself into read-only PLAN mode before starting a large, risky, \
                or ambiguous task. In plan mode all edits and shell commands are blocked, so you \
                research and design first. When your plan is ready, call `exit_plan` to present it \
                for the user's approval; only after they approve are edits unblocked. Use this \
                proactively when a task clearly needs a plan first — don't start editing blind. \
                Skip it for small, clear changes you can just make."
                    .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn execute(&self, _input: Value, ctx: &ToolContext) -> ToolResult {
        use crate::core::permissions::Mode;
        match &ctx.permissions {
            Some(perms) => {
                if perms.mode() == Mode::Plan {
                    return Ok(
                        "already in plan mode. Research, then call exit_plan with your plan."
                            .to_string(),
                    );
                }
                perms.set_mode(Mode::Plan);
                Ok(
                    "entered plan mode (read-only). Research the code, then call exit_plan with \
                    your proposed plan for approval. Edits stay blocked until then."
                        .to_string(),
                )
            }
            None => Err(ToolError::unavailable(
                "plan mode is not available in this context",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::plan_slug;

    #[test]
    fn slug_uses_first_heading() {
        let s = plan_slug("# Add web search tool\n\nSome body");
        assert!(s.starts_with("add-web-search-tool-"), "got {s}");
    }

    #[test]
    fn slug_is_stable_for_same_plan() {
        let plan = "# Refactor\n\nstep 1";
        assert_eq!(plan_slug(plan), plan_slug(plan));
    }

    #[test]
    fn slug_differs_when_body_differs() {
        assert_ne!(plan_slug("# Same\n\nA"), plan_slug("# Same\n\nB"));
    }

    #[test]
    fn slug_falls_back_without_heading() {
        let s = plan_slug("just some text, no heading");
        assert!(s.starts_with("just-some-text-"), "got {s}");
    }

    #[test]
    fn slug_handles_empty_and_symbol_only() {
        assert!(plan_slug("").starts_with("plan-"));
        assert!(plan_slug("###   ").starts_with("plan-"));
    }
}
