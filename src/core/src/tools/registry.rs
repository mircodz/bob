//! Validates tool inputs and applies configured permissions before execution.

use crate::core::permissions::{ParsedCommand, PermissionEngine, PermissionRequest};
use crate::core::types::{ToolOutput, ToolSpec};
use crate::tools::file_tracker::FileTracker;
use crate::tools::jobs::JobRegistry;
use crate::tools::todo::TodoStore;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct UserQuery {
    pub title: String,
    pub detail: String,
    pub options: Vec<String>,
    /// If true, the UI offers a free-text "Other…" choice.
    pub allow_other: bool,
}

/// Returns an option label or free text; `None` means the user dismissed it.
#[async_trait]
pub trait UserAsker: Send + Sync {
    async fn ask(&self, query: &UserQuery) -> Option<String>;
}

/// Shared per-session state handed to every tool call.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: String,
    pub files: Arc<FileTracker>,
    pub todos: Arc<TodoStore>,
    pub jobs: JobRegistry,
    /// Interaction tools return Unavailable when no UI hook is supplied.
    pub user_asker: Option<Arc<dyn UserAsker>>,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    pub coord: Option<CoordContext>,
    /// Supplied by the registry; mode-changing tools use the same engine.
    pub permissions: Option<Arc<PermissionEngine>>,
}

#[derive(Clone)]
pub struct CoordContext {
    pub name: String,
    /// Root is 0; children are `depth + 1`.
    pub depth: usize,
    pub team: crate::agent::team::AgentRegistry,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult;
    /// True only for tools that do not mutate workspace or shared state.
    /// Controls concurrency and explore membership; auto-allow is a separate policy.
    fn is_read_only(&self) -> bool {
        false
    }
    fn command_analysis(&self, _input: &Value) -> Option<ParsedCommand> {
        None
    }
    /// Permission-prompt preview; must not have side effects.
    fn preview(&self, _input: &Value, _ctx: &ToolContext) -> Option<String> {
        None
    }
}

/// Failure is determined by the variant, never by the output text.
pub type ToolResult = Result<ToolOutput, ToolError>;

pub(crate) const SHELL_STREAM_TEXT_LIMIT: usize = 12_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolErrorKind {
    NotFound,
    InvalidInput,
    PermissionDenied,
    Timeout,
    /// A required facility is absent (LSP, UI, team).
    Unavailable,
    /// Unclassified runtime failure.
    Failed,
    Panic,
}

impl ToolErrorKind {
    /// Stable snake_case slug, used as the wire prefix the model sees.
    pub fn slug(self) -> &'static str {
        match self {
            ToolErrorKind::NotFound => "not_found",
            ToolErrorKind::InvalidInput => "invalid_input",
            ToolErrorKind::PermissionDenied => "permission_denied",
            ToolErrorKind::Timeout => "timeout",
            ToolErrorKind::Unavailable => "unavailable",
            ToolErrorKind::Failed => "failed",
            ToolErrorKind::Panic => "panic",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
    pub output: Option<ToolOutput>,
}

impl ToolError {
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        ToolError {
            kind,
            message: message.into(),
            output: None,
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::NotFound, message)
    }
    pub fn invalid_input(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::InvalidInput, message)
    }
    pub fn permission_denied(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::PermissionDenied, message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::Timeout, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::Unavailable, message)
    }
    pub fn failed(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::Failed, message)
    }

    pub fn with_output(mut self, output: impl Into<ToolOutput>) -> Self {
        self.output = Some(output.into());
        self
    }

    pub fn into_output(self) -> ToolOutput {
        self.output
            .unwrap_or_else(|| ToolOutput::Text(format!("{}: {}", self.kind.slug(), self.message)))
    }

    /// Model-visible wire format: `"<kind>: <message>"`.
    pub fn wire(&self) -> String {
        format!("{}: {}", self.kind.slug(), self.message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.wire())
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => ToolError::not_found(e.to_string()),
            ErrorKind::PermissionDenied => ToolError::permission_denied(e.to_string()),
            _ => ToolError::failed(e.to_string()),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
    schemas: HashMap<String, Result<Arc<crate::tools::schema::CompiledSchema>, String>>,
    permissions: Option<Arc<PermissionEngine>>,
}

impl ToolRegistry {
    pub fn new(permissions: Option<Arc<PermissionEngine>>) -> Self {
        ToolRegistry {
            tools: HashMap::new(),
            order: Vec::new(),
            schemas: HashMap::new(),
            permissions,
        }
    }

    /// First registration wins; collisions return false. Register trusted tools
    /// before MCP/extra tools so untrusted names cannot inherit built-in trust.
    pub fn add(&mut self, tool: Arc<dyn Tool>) -> bool {
        let spec = tool.spec();
        let name = spec.name;
        if self.tools.contains_key(&name) {
            return false;
        }
        self.schemas.insert(
            name.clone(),
            crate::tools::schema::CompiledSchema::compile(&spec.input_schema).map(Arc::new),
        );
        self.order.push(name.clone());
        self.tools.insert(name, tool);
        true
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Unknown tools are conservatively treated as mutating.
    pub fn is_read_only(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.is_read_only())
    }

    /// Confines explore agents to tools marked read-only.
    pub fn read_only_subset(&self) -> ToolRegistry {
        let mut out = ToolRegistry::new(self.permissions.clone());
        for name in &self.order {
            if let Some(t) = self.tools.get(name) {
                if t.is_read_only() {
                    out.add(t.clone());
                }
            }
        }
        out
    }

    /// Applies a subagent's tool allow-list; unknown names are skipped.
    pub fn subset(&self, names: &[String]) -> ToolRegistry {
        let mut out = ToolRegistry::new(self.permissions.clone());
        for name in &self.order {
            if names.iter().any(|n| n == name) {
                if let Some(t) = self.tools.get(name) {
                    out.add(t.clone());
                }
            }
        }
        out
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|n| self.tools.get(n).map(|t| t.spec()))
            .collect()
    }

    pub async fn execute(&self, name: &str, input: Value, ctx: &ToolContext) -> ToolResult {
        crate::core::panic::catch(self.execute_inner(name, input, ctx))
            .await
            .unwrap_or_else(|_| Err(ToolError::new(
                ToolErrorKind::Panic,
                "tool panicked; outcome unknown, changes may have taken effect; do not automatically retry",
            )))
    }

    async fn execute_inner(&self, name: &str, input: Value, ctx: &ToolContext) -> ToolResult {
        let tool = match self.tools.get(name) {
            Some(t) => t.clone(),
            None => return Err(ToolError::not_found(format!("unknown tool \"{}\"", name))),
        };

        let schema = self
            .schemas
            .get(name)
            .expect("registered tool has schema")
            .as_ref()
            .map_err(|error| ToolError::invalid_input(format!("invalid tool schema: {error}")))?;
        schema.validate(&input).map_err(ToolError::invalid_input)?;

        // Recheck path resolution after human approval; a symlink may have moved.
        let path_snapshot = if matches!(
            name,
            "read_file"
                | "write_file"
                | "edit_file"
                | "multi_edit"
                | "list_dir"
                | "glob"
                | "grep"
                | "lsp"
                | "rename_symbol"
                | "code_action"
        ) {
            input
                .get("filePath")
                .or_else(|| input.get("path"))
                .and_then(Value::as_str)
                .or_else(|| matches!(name, "list_dir" | "glob" | "grep").then_some("."))
                .map(|path| {
                    let resolved = crate::core::policies::resolve_workspace_path(
                        std::path::Path::new(&ctx.cwd),
                        std::path::Path::new(path),
                    )?;
                    Ok::<_, std::io::Error>((path.to_owned(), resolved))
                })
                .transpose()?
        } else {
            None
        };

        if let Some(perms) = &self.permissions {
            let bash = tool.command_analysis(&input);
            let preview = tool.preview(&input, ctx);
            let req = PermissionRequest {
                tool: name.to_string(),
                input: input.clone(),
                cwd: ctx.cwd.clone(),
                bash,
                preview,
                agent: ctx
                    .coord
                    .as_ref()
                    .map(|c| c.name.clone())
                    .filter(|n| n != crate::agent::agent::ROOT_AGENT_ID),
                read_only: tool.is_read_only(),
            };
            if !perms.check(&req).await {
                return Err(ToolError::permission_denied(format!(
                    "permission denied for tool \"{}\"",
                    name
                )));
            }
        }

        if let Some((path, approved)) = path_snapshot {
            let current = crate::core::policies::resolve_workspace_path(
                std::path::Path::new(&ctx.cwd),
                std::path::Path::new(&path),
            )?;
            if current != approved {
                return Err(ToolError::permission_denied(
                    "path target changed during approval; retry for the new target",
                ));
            }
        }
        let mut ctx = ctx.clone();
        ctx.permissions = self.permissions.clone();
        tool.execute(input, &ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture;
    use crate::tools::builtin::ReadFileTool;
    use serde_json::json;

    struct StubTool {
        name: &'static str,
        tag: &'static str,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
            }
        }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            Ok(self.tag.into())
        }
    }

    struct ValidatedTool {
        schema: Value,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        previews: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for ValidatedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "validated".into(),
                description: String::new(),
                input_schema: self.schema.clone(),
            }
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("executed".into())
        }
        fn preview(&self, _: &Value, _: &ToolContext) -> Option<String> {
            self.previews
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some("preview".into())
        }
    }

    #[tokio::test]
    async fn malformed_tool_inputs_are_rejected_before_execution() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let previews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let permissions = Arc::new(PermissionEngine::new(
            crate::core::permissions::Decision::Allow,
            None,
        ));
        let mut registry = ToolRegistry::new(Some(permissions));
        registry.add(Arc::new(ValidatedTool {
            schema: json!({
                "type":"object", "required":["content"],
                "properties":{"content":{"type":"string"}}, "additionalProperties":false
            }),
            calls: calls.clone(),
            previews: previews.clone(),
        }));
        let fixture = FileFixture::new();
        let ctx = &fixture.ctx;
        for input in [
            Value::Null,
            json!([]),
            json!("not an object"),
            json!({}),
            json!({"content":null}),
            json!({"content":12}),
            json!({"content":"ok","extra":true}),
        ] {
            assert_eq!(
                registry
                    .execute("validated", input, ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(previews.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            registry
                .execute("validated", json!({"content":""}), ctx)
                .await
                .unwrap()
                .as_text(),
            Some("executed")
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(previews.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn first_registration_wins_and_executes_retained_builtin() {
        let fixture = FileFixture::new();
        fixture.write("file", "trusted builtin content");
        let mut reg = ToolRegistry::new(None);
        assert!(reg.add(Arc::new(ReadFileTool)));
        assert!(!reg.add(Arc::new(StubTool {
            name: "read_file",
            tag: "mcp-impostor",
        })));
        assert_eq!(
            reg.specs().iter().filter(|s| s.name == "read_file").count(),
            1
        );
        let output = reg
            .execute("read_file", json!({"path":"file"}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(
            output.model_text().lines().next(),
            Some("     1\ttrusted builtin content")
        );
        assert!(fixture.ctx.files.fully_read(
            &fixture.dir.join("file").to_string_lossy(),
            "trusted builtin content"
        ));
        assert_eq!(
            reg.execute("read_file", json!({}), &fixture.ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn invalid_schema_unknown_tool_and_permission_denial_never_execute() {
        use crate::core::permissions::Decision;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = FileFixture::new();
        for (schema, decision, expected, expected_previews) in [
            (
                json!({"type":"not-a-type"}),
                Decision::Allow,
                ToolErrorKind::InvalidInput,
                0,
            ),
            (
                json!({"$ref":"https://example.invalid/schema"}),
                Decision::Allow,
                ToolErrorKind::InvalidInput,
                0,
            ),
            (
                json!({"type":"object"}),
                Decision::Deny,
                ToolErrorKind::PermissionDenied,
                1,
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let previews = Arc::new(AtomicUsize::new(0));
            let mut registry =
                ToolRegistry::new(Some(Arc::new(PermissionEngine::new(decision, None))));
            registry.add(Arc::new(ValidatedTool {
                schema,
                calls: calls.clone(),
                previews: previews.clone(),
            }));
            let error = registry
                .execute("validated", json!({}), &fixture.ctx)
                .await
                .unwrap_err();
            assert_eq!(error.kind, expected);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(previews.load(Ordering::SeqCst), expected_previews);
            let error = registry
                .execute("missing", json!({}), &fixture.ctx)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::NotFound);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(previews.load(Ordering::SeqCst), expected_previews);
        }
    }

    #[tokio::test]
    async fn subsets_preserve_registration_order_identity_and_permission_gating() {
        use crate::core::permissions::Decision;
        use crate::tools::web::WebFetchTool;

        let permissions = Arc::new(PermissionEngine::new(Decision::Deny, None));
        let mut registry = ToolRegistry::new(Some(permissions.clone()));
        registry.add(Arc::new(ReadFileTool));
        registry.add(Arc::new(StubTool {
            name: "mutating",
            tag: "must not run",
        }));
        registry.add(Arc::new(WebFetchTool));
        let readonly = registry.read_only_subset();
        let selected = registry.subset(&[
            "web_fetch".into(),
            "missing".into(),
            "mutating".into(),
            "web_fetch".into(),
        ]);
        assert_eq!(
            readonly
                .specs()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["read_file", "web_fetch"]
        );
        assert_eq!(
            selected
                .specs()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["mutating", "web_fetch"]
        );
        assert!(!registry.is_read_only("missing"));
        assert!(!registry.is_read_only("mutating"));
        assert!(registry.is_read_only("read_file"));
        assert!(readonly.get("mutating").is_none());
        assert!(Arc::ptr_eq(
            &selected.get("web_fetch").unwrap(),
            &registry.get("web_fetch").unwrap()
        ));
        for subset in [&readonly, &selected] {
            assert!(Arc::ptr_eq(
                subset.permissions.as_ref().unwrap(),
                &permissions
            ));
        }
        let fixture = FileFixture::new();
        assert_eq!(
            selected
                .execute("mutating", json!({}), &fixture.ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::PermissionDenied
        );
    }

    struct CommandProbe(crate::tools::bash::BashTool);

    #[async_trait]
    impl Tool for CommandProbe {
        fn spec(&self) -> ToolSpec {
            self.0.spec()
        }

        fn command_analysis(&self, input: &Value) -> Option<ParsedCommand> {
            self.0.command_analysis(input)
        }

        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            Ok("executed".into())
        }
    }

    #[tokio::test]
    async fn powershell_requires_fresh_approval_without_inheriting_bash_grants() {
        use crate::core::permissions::{Asker, Decision, Grant, Mode, PermissionOption};
        use crate::core::policies::{allow_bash_commands, deny_tools, flag_dangerous_bash};
        use crate::core::shell::{CommandShell, ShellKind};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        #[derive(Default)]
        struct Approval {
            allow: AtomicBool,
            calls: AtomicUsize,
        }
        #[async_trait]
        impl Asker for Approval {
            async fn ask(
                &self,
                req: &PermissionRequest,
                options: &[PermissionOption],
            ) -> Option<usize> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                assert!(req.bash.is_none());
                assert!(options.iter().all(|option| option.grant.is_none()));
                options
                    .iter()
                    .position(|option| option.allow == self.allow.load(Ordering::SeqCst))
            }
        }

        let fixture = FileFixture::new();
        let approval = Arc::new(Approval::default());
        let mut engine = PermissionEngine::new(Decision::Ask, Some(approval.clone()));
        engine.add(allow_bash_commands(vec!["git".into()]));
        engine.add(flag_dangerous_bash());
        engine.import_grants(vec![
            Grant::Tool {
                name: "bash".into(),
            },
            Grant::BashCommand {
                name: "git".into(),
                glob: "**".into(),
            },
        ]);
        let engine = Arc::new(engine);
        let probe = |kind| {
            Arc::new(CommandProbe(crate::tools::bash::BashTool::new(Arc::new(
                CommandShell::for_test(fixture.dir.join("not-launched.exe"), kind),
            ))))
        };
        let mut bash = ToolRegistry::new(Some(engine.clone()));
        bash.add(probe(ShellKind::Bash));
        let mut pwsh = ToolRegistry::new(Some(engine.clone()));
        pwsh.add(probe(ShellKind::Pwsh));
        let input = json!({"command": "git status"});
        assert!(bash
            .execute("bash", input.clone(), &fixture.ctx)
            .await
            .is_ok());
        assert_eq!(approval.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            pwsh.execute("bash", input.clone(), &fixture.ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::PermissionDenied
        );

        approval.allow.store(true, Ordering::SeqCst);
        for _ in 0..2 {
            assert!(pwsh
                .execute("bash", input.clone(), &fixture.ctx)
                .await
                .is_ok());
        }
        assert_eq!(approval.calls.load(Ordering::SeqCst), 3);
        assert_eq!(engine.export_grants().len(), 2);
        engine.toggle_bypass();
        assert!(pwsh
            .execute("bash", input.clone(), &fixture.ctx)
            .await
            .is_ok());
        assert_eq!(approval.calls.load(Ordering::SeqCst), 3);
        engine.set_mode(Mode::Plan);
        assert_eq!(
            pwsh.execute("bash", input.clone(), &fixture.ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::PermissionDenied
        );

        let mut denied = PermissionEngine::new(Decision::Allow, None);
        denied.add(deny_tools(vec!["bash".into()]));
        denied.toggle_bypass();
        let mut denied_registry = ToolRegistry::new(Some(Arc::new(denied)));
        denied_registry.add(probe(ShellKind::Pwsh));
        assert_eq!(
            denied_registry
                .execute("bash", input, &fixture.ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::PermissionDenied
        );
    }

    struct PanicTool(bool);

    #[async_trait]
    impl Tool for PanicTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "panic_tool".into(),
                description: String::new(),
                input_schema: json!({"type":"object"}),
            }
        }
        fn preview(&self, _: &Value, _: &ToolContext) -> Option<String> {
            assert!(!self.0, "preview panic");
            None
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            tokio::task::yield_now().await;
            panic!("async polling panic");
        }
    }

    #[tokio::test]
    async fn catches_preview_and_async_poll_panics_as_distinct_failures() {
        let fixture = FileFixture::new();
        for preview in [true, false] {
            let mut registry = ToolRegistry::new(Some(Arc::new(PermissionEngine::new(
                crate::core::permissions::Decision::Allow,
                None,
            ))));
            registry.add(Arc::new(PanicTool(preview)));
            let error = registry
                .execute("panic_tool", json!({}), &fixture.ctx)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::Panic);
            assert!(error.message.contains("outcome unknown"));
            assert!(error.message.contains("do not automatically retry"));
        }
    }

    #[test]
    fn io_errors_keep_actionable_categories_and_wire_prefixes() {
        for (io_kind, kind, slug) in [
            (
                std::io::ErrorKind::NotFound,
                ToolErrorKind::NotFound,
                "not_found",
            ),
            (
                std::io::ErrorKind::PermissionDenied,
                ToolErrorKind::PermissionDenied,
                "permission_denied",
            ),
            (std::io::ErrorKind::Other, ToolErrorKind::Failed, "failed"),
        ] {
            let error = ToolError::from(std::io::Error::new(io_kind, "detail"));
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, "detail");
            assert_eq!(error.wire(), format!("{slug}: detail"));
            assert_eq!(error.to_string(), error.wire());
        }
    }
}
