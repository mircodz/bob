//! User-interaction tools: `ask_user` (pose a question with options) and
//! `exit_plan` (present a plan for approval). Both delegate to the UI via the
//! `UserAsker` hook on the ToolContext.

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, UserQuery};
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
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
                Some(answer) => format!("user answered: {}", answer),
                None => "user dismissed the question without answering".to_string(),
            },
            None => "error: no interactive UI available to ask the user".to_string(),
        }
    }
}

/// `exit_plan`: present the plan bob has formed and ask the user to approve it
/// (or request changes). Called at the end of plan mode. Returns the user's
/// decision so bob knows whether to proceed or keep refining.
pub struct ExitPlanTool;

#[async_trait]
impl Tool for ExitPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_plan".to_string(),
            description: "Present your implementation plan and ask the user to approve it before \
                you start making changes. Call this ONLY in plan mode, once you've researched \
                enough to propose concrete steps. Pass the plan as Markdown. The user will either \
                approve (you may then proceed and edits are unblocked) or ask you to refine it."
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let plan = input["plan"].as_str().unwrap_or("").to_string();
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
                    // just relay the user's words back to the model.
                    format!("user responded: {}", answer)
                }
                None => "user dismissed the plan approval".to_string(),
            },
            None => "error: no interactive UI available to present the plan".to_string(),
        }
    }
}
