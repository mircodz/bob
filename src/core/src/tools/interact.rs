use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult, UserQuery};
use async_trait::async_trait;
use serde_json::{json, Value};

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
                Some(answer) => Ok(format!("user answered: {}", answer).into()),
                None => Ok("user dismissed the question without answering".into()),
            },
            None => Err(ToolError::unavailable(
                "no interactive UI available to ask the user",
            )),
        }
    }
}

pub struct ExitPlanTool;

/// Filesystem-safe title with a content hash to distinguish plan revisions.
fn plan_slug(plan: &str) -> String {
    use sha2::{Digest, Sha256};
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

/// Persistence is best-effort; failure must not block approval.
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
                    // The UI owns the mode switch; this tool only relays the answer.
                    match saved {
                        Some(path) => Ok(format!(
                            "user responded: {}\n(plan saved to {})",
                            answer,
                            path.display()
                        )
                        .into()),
                        None => Ok(format!("user responded: {}", answer).into()),
                    }
                }
                None => Ok("user dismissed the plan approval".into()),
            },
            None => Err(ToolError::unavailable(
                "no interactive UI available to present the plan",
            )),
        }
    }
}

pub struct EnterPlanTool;

#[async_trait]
impl Tool for EnterPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "enter_plan".to_string(),
            description:
                "Enter read-only Plan mode for a large, risky, or underspecified implementation, \
                or when the user requests it. Edits and shell commands are blocked until approval \
                through exit_plan. Research with permitted read/search tools and present concrete \
                implementation steps. Skip this extra approval flow for a research-only question, \
                a small clear change, or work whose plan the user already approved."
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
                            .into(),
                    );
                }
                perms.set_mode(Mode::Plan);
                Ok(
                    "entered plan mode (read-only). Research the code, then call exit_plan with \
                    your proposed plan for approval. Edits stay blocked until then."
                        .into(),
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
    use super::*;
    use crate::test_support::{files::tool_context, TestDir};
    use crate::tools::registry::UserAsker;
    use std::sync::{Arc, Mutex};

    #[test]
    fn plan_filenames_use_heading_fallback_and_empty_title_with_fixed_hashes() {
        for (plan, expected) in [
            (
                "# Add web search tool\n\nSome body",
                "add-web-search-tool-90c5e26c.md",
            ),
            (
                "preamble\n## First Heading\n# Second",
                "first-heading-2dc2df48.md",
            ),
            (
                "just some text, no heading",
                "just-some-text-no-heading-94876bd9.md",
            ),
            ("", "plan-e3b0c442.md"),
            ("###   ", "plan-29e6ee3d.md"),
            ("\n  \n", "plan-294f590e.md"),
            ("# ../Ä__Safe / name!?", "safe-name-0dd9aca6.md"),
        ] {
            assert_eq!(format!("{}.md", plan_slug(plan)), expected, "{plan:?}");
        }
    }

    #[test]
    fn plan_filename_identity_includes_body_not_only_title() {
        assert_eq!(
            format!("{}.md", plan_slug("# Same\n\nA")),
            "same-555ebb1f.md"
        );
        assert_eq!(
            format!("{}.md", plan_slug("# Same\n\nB")),
            "same-643c07ca.md"
        );
    }

    struct RecordingAsker {
        answer: Option<String>,
        queries: Mutex<Vec<UserQuery>>,
    }

    #[async_trait]
    impl UserAsker for RecordingAsker {
        async fn ask(&self, query: &UserQuery) -> Option<String> {
            self.queries.lock().unwrap().push(query.clone());
            self.answer.clone()
        }
    }

    #[tokio::test]
    async fn ask_user_routes_query_and_distinguishes_answer_dismissal_and_no_ui() {
        let dir = TestDir::new();
        let mut ctx = tool_context(dir.path());
        let input = json!({"question": "Which?", "detail": "Context", "options": ["One", "Two"]});
        assert_eq!(
            AskUserTool
                .execute(input.clone(), &ctx)
                .await
                .unwrap_err()
                .kind,
            crate::tools::registry::ToolErrorKind::Unavailable
        );
        for (answer, expected) in [
            (
                Some("custom answer".to_string()),
                "user answered: custom answer",
            ),
            (None, "user dismissed the question without answering"),
        ] {
            let asker = Arc::new(RecordingAsker {
                answer,
                queries: Mutex::new(Vec::new()),
            });
            ctx.user_asker = Some(asker.clone());
            assert_eq!(
                AskUserTool
                    .execute(input.clone(), &ctx)
                    .await
                    .unwrap()
                    .as_text()
                    .unwrap(),
                expected
            );
            let queries = asker.queries.lock().unwrap();
            assert_eq!(queries.len(), 1);
            assert_eq!(queries[0].title, "Which?");
            assert_eq!(queries[0].detail, "Context");
            assert_eq!(queries[0].options, ["One", "Two"]);
            assert!(queries[0].allow_other);
        }
    }

    #[tokio::test]
    async fn registered_question_rejects_malformed_input_before_asking() {
        use crate::tools::registry::{ToolErrorKind, ToolRegistry};

        let dir = TestDir::new();
        let mut ctx = tool_context(dir.path());
        let asker = Arc::new(RecordingAsker {
            answer: Some("One".into()),
            queries: Mutex::new(Vec::new()),
        });
        ctx.user_asker = Some(asker.clone());
        let mut registry = ToolRegistry::new(None);
        registry.add(Arc::new(AskUserTool));
        for input in [
            json!({"options":["One", "Two"]}),
            json!({"question":"Which?"}),
            json!({"question":null, "options":["One", "Two"]}),
            json!({"question":"Which?", "options":["One", 2]}),
            json!({"question":"Which?", "options":"One"}),
            json!({"question":"Which?", "options":["One", "Two"], "detail":false}),
        ] {
            assert_eq!(
                registry
                    .execute("ask_user", input, &ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
            assert!(asker.queries.lock().unwrap().is_empty());
        }
        assert_eq!(
            registry
                .execute(
                    "ask_user",
                    json!({"question":"Which?", "options":["One", "Two"]}),
                    &ctx
                )
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "user answered: One"
        );
        let queries = asker.queries.lock().unwrap();
        assert_eq!(queries.len(), 1);
        assert!(queries[0].detail.is_empty());
    }

    #[tokio::test]
    async fn enter_plan_changes_mode_and_is_idempotent() {
        use crate::core::permissions::{Decision, Mode, PermissionEngine};
        use crate::tools::registry::ToolErrorKind;

        let dir = TestDir::new();
        let mut ctx = tool_context(dir.path());
        assert_eq!(
            EnterPlanTool
                .execute(json!({}), &ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::Unavailable
        );
        let permissions = Arc::new(PermissionEngine::new(Decision::Allow, None));
        ctx.permissions = Some(permissions.clone());
        assert!(EnterPlanTool
            .execute(json!({}), &ctx)
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .starts_with("entered plan mode"));
        assert_eq!(permissions.mode(), Mode::Plan);
        assert!(EnterPlanTool
            .execute(json!({}), &ctx)
            .await
            .unwrap()
            .as_text()
            .unwrap()
            .starts_with("already in plan mode"));
        assert_eq!(permissions.mode(), Mode::Plan);
    }

    #[test]
    fn long_plan_titles_are_bounded_without_a_trailing_separator() {
        let plan = format!("# {} rest", "a".repeat(47));
        let slug = plan_slug(&plan);
        let (title, hash) = slug.rsplit_once('-').unwrap();
        assert_eq!(title, "a".repeat(47));
        assert_eq!(hash.len(), 8);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(slug, plan_slug(&plan));
    }
}
