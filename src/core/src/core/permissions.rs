//! Hard denials override rules and grants. YOLO bypasses approvals, not denials or plan mode.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// A hard policy veto, including in bypass mode.
    Deny,
    Ask,
    /// Require one-shot approval unless YOLO is enabled.
    RequireApproval,
}

pub struct PermissionRequest {
    pub tool: String,
    pub input: Value,
    pub cwd: String,
    pub bash: Option<ParsedCommand>,
    /// Side-effect-free preview shown before approval.
    pub preview: Option<String>,
    /// Requester attribution; `None` for the root agent.
    pub agent: Option<String>,
    /// Supplied by `Tool::is_read_only`; unknown mutators stay blocked in plan mode.
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct PermissionOption {
    pub label: String,
    pub allow: bool,
    /// `None` for a one-shot decision.
    pub grant: Option<Grant>,
}

/// Persisted with the session so approvals survive resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grant {
    Tool {
        name: String,
    },
    /// `"**"` permits any args; other globs constrain every path argument.
    BashCommand {
        name: String,
        glob: String,
    },
}

/// Return a choice index; `None` or an invalid index denies the request.
#[async_trait::async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, req: &PermissionRequest, options: &[PermissionOption]) -> Option<usize>;
}

/// `None` abstains from the decision.
pub type Rule = Arc<dyn Fn(&PermissionRequest) -> Option<Decision> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Auto-allow file edits, not shell commands.
    AutoAccept,
    /// Block mutating tools regardless of YOLO.
    Plan,
}

impl Mode {
    pub fn as_u8(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::AutoAccept => 1,
            Mode::Plan => 2,
        }
    }
    pub fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::AutoAccept,
            2 => Mode::Plan,
            _ => Mode::Normal,
        }
    }
    pub fn next(self) -> Mode {
        match self {
            Mode::Normal => Mode::AutoAccept,
            Mode::AutoAccept => Mode::Plan,
            Mode::Plan => Mode::Normal,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::AutoAccept => "auto-accept edits",
            Mode::Plan => "plan",
        }
    }
}

// These interactions do not modify the workspace.
fn is_plan_safe_tool(name: &str) -> bool {
    matches!(
        name,
        "enter_plan" | "exit_plan" | "ask_user" | "todo_write" | "memory"
    )
}

fn is_file_edit_tool(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "multi_edit")
}

pub struct PermissionEngine {
    default: Decision,
    rules: Vec<Rule>,
    asker: Option<Arc<dyn Asker>>,
    grants: Mutex<Vec<Grant>>,
    mode: std::sync::atomic::AtomicU8,
    // Separate from mode so cycling modes cannot enable YOLO.
    bypass: std::sync::atomic::AtomicBool,
}

impl PermissionEngine {
    pub fn new(default: Decision, asker: Option<Arc<dyn Asker>>) -> Self {
        PermissionEngine {
            default,
            rules: Vec::new(),
            asker,
            grants: Mutex::new(Vec::new()),
            mode: std::sync::atomic::AtomicU8::new(Mode::Normal.as_u8()),
            bypass: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode
            .store(mode.as_u8(), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Returns the new bypass state.
    pub fn toggle_bypass(&self) -> bool {
        let next = !self.bypass();
        self.bypass
            .store(next, std::sync::atomic::Ordering::Relaxed);
        next
    }

    pub fn bypass(&self) -> bool {
        self.bypass.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn add(&mut self, rule: Rule) -> &mut Self {
        self.rules.push(rule);
        self
    }

    pub async fn check(&self, req: &PermissionRequest) -> bool {
        let mode = self.mode();

        if mode == Mode::Plan && !req.read_only && !is_plan_safe_tool(&req.tool) {
            return false;
        }

        let mut first = None;
        let mut mandatory = false;
        for rule in &self.rules {
            if let Some(decision) = rule(req) {
                match decision {
                    Decision::Deny => return false,
                    Decision::RequireApproval => mandatory = true,
                    Decision::Allow | Decision::Ask => {
                        first.get_or_insert(decision);
                    }
                }
            }
        }
        let decision = if mandatory {
            Decision::RequireApproval
        } else {
            first.unwrap_or(self.default)
        };
        if decision == Decision::Deny {
            return false;
        }
        if self.bypass() {
            return true;
        }

        if decision != Decision::RequireApproval
            && ((mode == Mode::AutoAccept && is_file_edit_tool(&req.tool))
                || self.grant_allows(req))
        {
            return true;
        }

        match decision {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask | Decision::RequireApproval => {
                let asker = match &self.asker {
                    Some(a) => a,
                    None => return false,
                };
                let mut options = build_options(req);
                if decision == Decision::RequireApproval {
                    options.retain(|option| option.grant.is_none());
                }
                match asker.ask(req, &options).await {
                    Some(i) if i < options.len() => {
                        let opt = &options[i];
                        if let Some(g) = &opt.grant {
                            self.grants.lock().unwrap().push(g.clone());
                        }
                        opt.allow
                    }
                    _ => false,
                }
            }
        }
    }

    fn grant_allows(&self, req: &PermissionRequest) -> bool {
        let grants = self.grants.lock().unwrap();
        grants.iter().any(|g| grant_matches(g, req))
    }

    pub fn export_grants(&self) -> Vec<Grant> {
        self.grants.lock().unwrap().clone()
    }

    pub fn import_grants(&self, grants: Vec<Grant>) {
        *self.grants.lock().unwrap() = grants;
    }
}

fn base_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

fn grant_matches(grant: &Grant, req: &PermissionRequest) -> bool {
    match grant {
        Grant::Tool { name } => {
            &req.tool == name && (req.tool != "bash" || req.bash.as_ref().is_some_and(is_simple))
        }
        Grant::BashCommand { name, glob } => {
            let Some(bash) = &req.bash else { return false };
            if !is_simple(bash) {
                return false;
            }
            let Some(argv) = bash.commands.first() else {
                return false;
            };
            let Some(cmd) = argv.first() else {
                return false;
            };
            if base_name(cmd) != name {
                return false;
            }
            if glob == "**" {
                return true;
            }
            // Match canonical targets too, not just spellings such as src/../other.
            let paths = path_args(argv);
            let Ok(root) = std::path::Path::new(&req.cwd).canonicalize() else {
                return false;
            };
            !paths.is_empty()
                && paths.iter().all(|path| {
                    let Ok(resolved) = crate::core::policies::resolve_workspace_path(
                        &root,
                        std::path::Path::new(path),
                    ) else {
                        return false;
                    };
                    let Ok(relative) = resolved.strip_prefix(&root) else {
                        return false;
                    };
                    let relative = relative
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/");
                    glob_match(glob, path) && glob_match(glob, &relative)
                })
        }
    }
}

// The analyzer flattens nested commands, so a single entry excludes hidden execution.
fn is_simple(bash: &ParsedCommand) -> bool {
    bash.analyzable && bash.commands.len() == 1 && !bash.pipes_to_shell && !bash.has_output_redirect
}

/// Conservatively rejects expansion and compound syntax beyond the allowlist's model.
pub fn has_shell_metachars(raw: &str) -> bool {
    const NEEDLES: &[&str] = &["$(", "${", "`", "<(", ">(", "<<", "$["];
    if NEEDLES.iter().any(|n| raw.contains(n)) {
        return true;
    }
    if raw.contains('\n') || raw.contains('\r') {
        return true;
    }
    raw.chars().any(|c| matches!(c, '(' | ')' | '{' | '}'))
}

fn path_args(argv: &[String]) -> Vec<String> {
    argv.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-') && !a.contains('='))
        .cloned()
        .collect()
}

fn build_options(req: &PermissionRequest) -> Vec<PermissionOption> {
    let mut opts = vec![PermissionOption {
        label: "Allow once".to_string(),
        allow: true,
        grant: None,
    }];

    if let Some(bash) = &req.bash {
        if is_simple(bash) {
            if let Some(argv) = bash.commands.first() {
                if let Some(cmd) = argv.first() {
                    let name = base_name(cmd).to_string();
                    let paths = path_args(argv);

                    if let Some(glob) = scope_glob(&paths) {
                        opts.push(PermissionOption {
                            label: format!("Always allow `{}` in {}", name, glob),
                            allow: true,
                            grant: Some(Grant::BashCommand {
                                name: name.clone(),
                                glob,
                            }),
                        });
                    }
                    opts.push(PermissionOption {
                        label: format!("Always allow `{}` (any args)", name),
                        allow: true,
                        grant: Some(Grant::BashCommand {
                            name,
                            glob: "**".to_string(),
                        }),
                    });
                }
            }
        }
    } else if req.tool != "bash" {
        opts.push(PermissionOption {
            label: format!("Always allow {}", req.tool),
            allow: true,
            grant: Some(Grant::Tool {
                name: req.tool.clone(),
            }),
        });
    }

    opts.push(PermissionOption {
        label: "Deny".to_string(),
        allow: false,
        grant: None,
    });
    opts
}

// Only offer a directory grant when every argument shares a safe top-level directory.
fn scope_glob(paths: &[String]) -> Option<String> {
    if paths.is_empty()
        || paths
            .iter()
            .any(|path| path.split('/').any(|part| part == ".."))
    {
        return None;
    }
    let first = paths[0].trim_start_matches("./");
    if !first.contains('/') {
        return None;
    }
    let top = first.split('/').next().unwrap_or(first);
    if top.is_empty() {
        return None;
    }
    let scope = format!("{}/**", top);
    if paths
        .iter()
        .all(|p| glob_match(&scope, p.trim_start_matches("./")))
    {
        Some(scope)
    } else {
        None
    }
}

/// Supports `*` within a segment and `**` across segments; other characters are literal.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn helper(p: &[u8], s: &[u8]) -> bool {
        if p.starts_with(b"**") {
            let rest = if p.len() > 2 && p[2] == b'/' {
                &p[3..]
            } else {
                &p[2..]
            };
            if rest.is_empty() {
                return true;
            }
            for i in 0..=s.len() {
                if helper(rest, &s[i..]) {
                    return true;
                }
            }
            return false;
        }
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if helper(&p[1..], s) {
                    return true;
                }
                if let Some(&c) = s.first() {
                    if c != b'/' {
                        return helper(p, &s[1..]);
                    }
                }
                false
            }
            (Some(&pc), Some(&sc)) if pc == sc => helper(&p[1..], &s[1..]),
            _ => false,
        }
    }
    helper(pattern.as_bytes(), path.as_bytes())
}

#[derive(Clone, Debug, Default)]
pub struct ParsedCommand {
    /// May be incomplete when `analyzable` is false.
    pub commands: Vec<Vec<String>>,
    /// Includes pipelines such as `curl … | sh`.
    pub pipes_to_shell: bool,
    /// Redirection can mutate files even when the command itself is read-only.
    pub has_output_redirect: bool,
    /// Literal input/output redirection targets, separate from argv.
    pub redirect_paths: Vec<String>,
    /// False for malformed or unmodeled syntax; allowlists must abstain.
    pub analyzable: bool,
    pub raw: String,
}

impl ParsedCommand {
    /// Distinct basenames in first-seen order; incomplete if analysis failed.
    pub fn program_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for argv in &self.commands {
            if let Some(cmd) = argv.first() {
                let name = base_name(cmd).to_string();
                if !name.is_empty() && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }
}

/// Parse failures and unmodeled constructs set `analyzable` to false.
pub fn parse_bash(raw: &str) -> ParsedCommand {
    match crate::core::bash_parse::analyze(raw) {
        Some(analysis) => ParsedCommand {
            commands: analysis.commands,
            pipes_to_shell: analysis.pipes_to_shell,
            has_output_redirect: analysis.has_output_redirect,
            redirect_paths: analysis.redirect_paths,
            analyzable: !analysis.has_dynamic,
            raw: raw.to_string(),
        },
        None => ParsedCommand {
            commands: Vec::new(),
            pipes_to_shell: false,
            has_output_redirect: false,
            redirect_paths: Vec::new(),
            analyzable: false,
            raw: raw.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn req_named(tool: &str) -> PermissionRequest {
        PermissionRequest {
            tool: tool.to_string(),
            input: serde_json::Value::Null,
            cwd: ".".to_string(),
            bash: None,
            preview: None,
            agent: None,
            read_only: false,
        }
    }

    fn bash_req(raw: &str) -> PermissionRequest {
        PermissionRequest {
            input: serde_json::json!({ "command": raw }),
            bash: Some(parse_bash(raw)),
            ..req_named("bash")
        }
    }

    struct CountingAsker {
        calls: AtomicUsize,
        allow: bool,
        fresh: bool,
    }

    #[async_trait::async_trait]
    impl Asker for CountingAsker {
        async fn ask(&self, _: &PermissionRequest, options: &[PermissionOption]) -> Option<usize> {
            if self.fresh {
                assert_eq!(options.len(), 2);
                assert!(options.iter().all(|option| option.grant.is_none()));
            }
            self.calls.fetch_add(1, Ordering::Relaxed);
            options.iter().position(|option| option.allow == self.allow)
        }
    }

    #[test]
    fn program_names_lists_distinct_programs() {
        assert_eq!(parse_bash("ls -la").program_names(), vec!["ls"]);
        let names = parse_bash("cat f | rm y && echo $(date)").program_names();
        assert_eq!(names.len(), 4);
        for p in ["cat", "rm", "echo", "date"] {
            assert!(names.contains(&p.to_string()), "missing {p} in {names:?}");
        }
        assert_eq!(
            parse_bash("/usr/bin/git status; git diff; /bin/cat file; git log").program_names(),
            vec!["git", "cat"]
        );
    }

    #[tokio::test]
    async fn ordinary_asks_follow_mode_and_bypass_without_an_asker() {
        let engine = PermissionEngine::new(Decision::Ask, None);
        let req = bash_req("printf hello");
        assert!(!engine.check(&req).await, "closed by default with no asker");
        assert!(engine.toggle_bypass(), "toggle returns new state (on)");
        assert!(engine.check(&req).await, "bypass allows the call");
        assert!(!engine.toggle_bypass(), "toggle again returns off");
        assert!(!engine.check(&req).await, "restored to fail-closed");
        engine.set_mode(Mode::AutoAccept);
        for tool in ["write_file", "edit_file", "multi_edit"] {
            assert!(engine.check(&req_named(tool)).await, "{tool}");
        }
        assert!(!engine.check(&req).await, "auto-accept does not cover bash");
    }

    #[tokio::test]
    async fn plan_mode_blocks_mutations_even_with_bypass() {
        for bypass in [false, true] {
            let engine = PermissionEngine::new(Decision::Allow, None);
            engine.set_mode(Mode::Plan);
            if bypass {
                engine.toggle_bypass();
            }
            for (tool, read_only, expected) in [
                ("write_file", false, false),
                ("memory", false, true),
                ("some_mcp_tool", false, false),
                ("read_file", true, true),
                ("exit_plan", false, true),
            ] {
                let mut request = req_named(tool);
                request.read_only = read_only;
                assert_eq!(engine.check(&request).await, expected, "{tool}, {bypass}");
                assert!(engine.export_grants().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn denials_override_modes_bypass_grants_and_rule_order_without_prompting() {
        for tool in [
            "write_file",
            "edit_file",
            "multi_edit",
            "read_file",
            "memory",
        ] {
            for mode in [Mode::Normal, Mode::AutoAccept, Mode::Plan] {
                for order in ["deny only", "deny first", "allow first"] {
                    for bypass in [false, true] {
                        for answer in [None, Some(false), Some(true)] {
                            let asker = answer.map(|allow| {
                                Arc::new(CountingAsker {
                                    calls: AtomicUsize::new(0),
                                    allow,
                                    fresh: false,
                                })
                            });
                            let mut engine = PermissionEngine::new(
                                Decision::Allow,
                                asker.clone().map(|asker| asker as Arc<dyn Asker>),
                            );
                            let allow = crate::core::policies::allow_tools(vec![tool.into()]);
                            let deny = crate::core::policies::deny_tools(vec![tool.into()]);
                            match order {
                                "deny only" => engine.add(deny),
                                "deny first" => engine.add(deny).add(allow),
                                "allow first" => engine.add(allow).add(deny),
                                _ => unreachable!(),
                            };
                            engine.set_mode(mode);
                            engine.import_grants(vec![Grant::Tool { name: tool.into() }]);
                            if bypass {
                                engine.toggle_bypass();
                            }
                            let mut request = req_named(tool);
                            request.read_only = tool == "read_file";
                            assert!(
                                !engine.check(&request).await,
                                "{tool}, {mode:?}, {order}, bypass={bypass}, answer={answer:?}"
                            );
                            if let Some(asker) = asker {
                                assert_eq!(asker.calls.load(Ordering::Relaxed), 0);
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn mandatory_approval_overrides_modes_grants_and_allow_rules() {
        for answer in [None, Some(false), Some(true)] {
            for mode in [Mode::Normal, Mode::AutoAccept] {
                let asker = answer.map(|allow| {
                    Arc::new(CountingAsker {
                        allow,
                        calls: AtomicUsize::new(0),
                        fresh: true,
                    })
                });
                let mut engine = PermissionEngine::new(
                    Decision::Allow,
                    asker.clone().map(|asker| asker as Arc<dyn Asker>),
                );
                engine.add(Arc::new(|_| Some(Decision::Allow)));
                engine.add(Arc::new(|_| Some(Decision::RequireApproval)));
                engine.set_mode(mode);
                engine.import_grants(vec![Grant::Tool {
                    name: "edit_file".into(),
                }]);
                for _ in 0..2 {
                    assert_eq!(
                        engine.check(&req_named("edit_file")).await,
                        answer.unwrap_or(false)
                    );
                }
                if let Some(asker) = asker {
                    assert_eq!(asker.calls.load(Ordering::Relaxed), 2);
                }
            }
        }
        let mut engine = PermissionEngine::new(Decision::Allow, None);
        engine.add(crate::core::policies::flag_dangerous_bash());
        engine.import_grants(vec![Grant::Tool {
            name: "bash".into(),
        }]);
        assert!(!engine.check(&bash_req("printf hello | sh")).await);
    }

    #[tokio::test]
    async fn yolo_bypasses_ordinary_and_mandatory_prompts_without_remembering_grants() {
        let fixture = TestDir::new();
        let cwd = fixture.join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        let outside = fixture.write("outside.txt", "outside");
        let mut requests = vec![
            bash_req("printf hello"),
            bash_req("git reset --hard"),
            PermissionRequest {
                input: serde_json::json!({"path": outside}),
                read_only: true,
                ..req_named("read_file")
            },
        ];
        for request in &mut requests {
            request.cwd = cwd.to_string_lossy().into_owned();
        }
        for mode in [Mode::Normal, Mode::AutoAccept] {
            let asker = Arc::new(CountingAsker {
                calls: AtomicUsize::new(0),
                allow: false,
                fresh: false,
            });
            let mut engine = PermissionEngine::new(Decision::Ask, Some(asker.clone()));
            engine.add(crate::core::policies::flag_out_of_workspace_paths());
            engine.add(crate::core::policies::flag_dangerous_bash());
            engine.add(crate::core::policies::allow_read_only());
            engine.set_mode(mode);
            for request in &requests {
                assert!(!engine.check(request).await);
            }
            assert_eq!(asker.calls.load(Ordering::Relaxed), requests.len());

            engine.toggle_bypass();
            for request in &requests {
                assert!(engine.check(request).await);
            }
            assert_eq!(asker.calls.load(Ordering::Relaxed), requests.len());
            assert!(engine.export_grants().is_empty());

            engine.toggle_bypass();
            for request in &requests {
                assert!(!engine.check(request).await);
            }
            assert_eq!(asker.calls.load(Ordering::Relaxed), requests.len() * 2);
            assert!(engine.export_grants().is_empty());
        }
    }

    #[tokio::test]
    async fn ordinary_risky_commands_remain_manually_approvable() {
        let asker = Arc::new(CountingAsker {
            calls: AtomicUsize::new(0),
            allow: true,
            fresh: false,
        });
        let mut engine = PermissionEngine::new(Decision::Ask, Some(asker.clone()));
        engine.add(crate::core::policies::flag_dangerous_bash());
        assert!(engine.check(&bash_req("git branch feature")).await);
        assert_eq!(asker.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn shell_grants_never_cover_output_redirection_or_unmodeled_syntax() {
        for grant in [
            Grant::Tool {
                name: "bash".into(),
            },
            Grant::BashCommand {
                name: "printf".into(),
                glob: "**".into(),
            },
        ] {
            let engine = PermissionEngine::new(Decision::Ask, None);
            engine.import_grants(vec![grant]);
            assert!(engine.check(&bash_req("printf hello")).await);
            for raw in [
                "printf hello > result",
                "printf hello >> result",
                "printf hello &> result",
                "printf hello >| result",
                "printf hello <> result",
                "printf hello 2>&1",
                "printf hello <<EOF\n$(printf hidden)\nEOF\n",
                "for item in $(printf hello); do printf ok; done",
            ] {
                let request = bash_req(raw);
                assert!(!engine.check(&request).await, "{raw}");
                assert!(
                    build_options(&request)
                        .iter()
                        .all(|option| option.grant.is_none()),
                    "{raw}"
                );
            }
        }
    }

    #[test]
    fn scoped_shell_grants_compare_canonical_paths() {
        let fixture = TestDir::new();
        fixture.write("src/lib.rs", "");
        fixture.write("src/nested/lib.rs", "");
        fixture.write("Cargo.toml", "");
        for (pattern, raw, expected) in [
            ("src/**", "cat src/lib.rs", true),
            ("src/**", "cat src/nested/lib.rs", true),
            ("src/**", "cat src/../Cargo.toml", false),
            ("src/*.rs", "cat src/lib.rs", true),
            ("src/*.rs", "cat src/new.rs", true),
            ("src/*.rs", "cat src/nested/lib.rs", false),
        ] {
            let grant = Grant::BashCommand {
                name: "cat".into(),
                glob: pattern.into(),
            };
            let mut request = bash_req(raw);
            request.cwd = fixture.to_string_lossy().into_owned();
            assert_eq!(
                grant_matches(&grant, &request),
                expected,
                "{pattern}: {raw}"
            );
        }
        assert!(scope_glob(&["src/../Cargo.toml".into()]).is_none());
    }

    #[tokio::test]
    async fn default_deny_is_hard_but_remains_a_fallback() {
        let mut engine = PermissionEngine::new(Decision::Deny, None);
        engine.toggle_bypass();
        assert!(!engine.check(&req_named("read_file")).await);
        engine.add(crate::core::policies::allow_tools(vec!["read_file".into()]));
        assert!(engine.check(&req_named("read_file")).await);
    }

    mod transitions {
        use super::*;
        use std::collections::VecDeque;

        enum Reply {
            Once,
            Deny,
            Cancel,
            Invalid,
            RememberTool,
            RememberSourceCommand,
        }

        struct ScriptedAsker {
            replies: Mutex<VecDeque<Reply>>,
            prompts: Mutex<Vec<Vec<PermissionOption>>>,
        }

        impl ScriptedAsker {
            fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
                Arc::new(Self {
                    replies: Mutex::new(replies.into_iter().collect()),
                    prompts: Mutex::new(Vec::new()),
                })
            }

            fn calls(&self) -> usize {
                self.prompts.lock().unwrap().len()
            }
        }

        #[async_trait::async_trait]
        impl Asker for ScriptedAsker {
            async fn ask(
                &self,
                _: &PermissionRequest,
                options: &[PermissionOption],
            ) -> Option<usize> {
                self.prompts.lock().unwrap().push(options.to_vec());
                let reply = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected prompt");
                match reply {
                    Reply::Cancel => None,
                    Reply::Invalid => Some(options.len()),
                    _ => Some(options.iter().position(|option| match reply {
                        Reply::Once => option.allow && option.grant.is_none(),
                        Reply::Deny => !option.allow && option.grant.is_none(),
                        Reply::RememberTool => matches!(&option.grant, Some(Grant::Tool { .. })),
                        Reply::RememberSourceCommand => matches!(
                            &option.grant,
                            Some(Grant::BashCommand { name, glob }) if name == "cat" && glob == "src/**"
                        ),
                        _ => unreachable!(),
                    }).expect("requested permission choice must be offered")),
                }
            }
        }

        #[test]
        fn permission_labels_describe_the_actual_decision_and_grant_scope() {
            let choices = |request: &PermissionRequest| {
                serde_json::json!(build_options(request)
                    .iter()
                    .map(|option| { (&option.label, option.allow, &option.grant) })
                    .collect::<Vec<_>>())
            };
            assert_eq!(
                choices(&req_named("edit_file")),
                serde_json::json!([
                    ["Allow once", true, null],
                    ["Always allow edit_file", true, {"kind":"tool","name":"edit_file"}],
                    ["Deny", false, null]
                ])
            );
            assert_eq!(
                choices(&bash_req("cat src/file.rs")),
                serde_json::json!([
                    ["Allow once", true, null],
                    ["Always allow `cat` in src/**", true, {"kind":"bash_command","name":"cat","glob":"src/**"}],
                    ["Always allow `cat` (any args)", true, {"kind":"bash_command","name":"cat","glob":"**"}],
                    ["Deny", false, null]
                ])
            );
        }

        #[tokio::test]
        async fn one_shot_denied_cancelled_and_invalid_answers_never_create_grants() {
            for (reply, allowed) in [
                (Reply::Once, true),
                (Reply::Deny, false),
                (Reply::Cancel, false),
                (Reply::Invalid, false),
            ] {
                let asker = ScriptedAsker::new([reply, Reply::Deny]);
                let engine = PermissionEngine::new(Decision::Ask, Some(asker.clone()));
                assert_eq!(engine.check(&req_named("edit_file")).await, allowed);
                assert!(engine.export_grants().is_empty());
                assert!(!engine.check(&req_named("edit_file")).await);
                assert_eq!(asker.calls(), 2);
                assert!(engine.export_grants().is_empty());
            }
        }

        #[tokio::test]
        async fn remembered_tool_survives_resume_but_not_revocation_or_another_tool() {
            let asker = ScriptedAsker::new([Reply::RememberTool, Reply::Deny]);
            let engine = PermissionEngine::new(Decision::Ask, Some(asker.clone()));
            assert!(engine.check(&req_named("edit_file")).await);
            assert!(engine.check(&req_named("edit_file")).await);
            assert!(!engine.check(&req_named("write_file")).await);
            assert_eq!(asker.calls(), 2);
            let saved = serde_json::to_value(engine.export_grants()).unwrap();
            assert_eq!(
                saved,
                serde_json::json!([{"kind": "tool", "name": "edit_file"}])
            );

            let resumed = PermissionEngine::new(Decision::Ask, None);
            resumed.import_grants(serde_json::from_value(saved).unwrap());
            assert!(resumed.check(&req_named("edit_file")).await);
            assert!(!resumed.check(&req_named("write_file")).await);
            resumed.import_grants(vec![Grant::Tool {
                name: "write_file".into(),
            }]);
            assert!(!resumed.check(&req_named("edit_file")).await);
            assert!(resumed.check(&req_named("write_file")).await);
            resumed.import_grants(vec![]);
            assert!(!resumed.check(&req_named("write_file")).await);
            assert!(resumed.export_grants().is_empty());
        }

        #[tokio::test]
        async fn scoped_choice_remembers_only_the_command_and_directory() {
            let fixture = TestDir::new();
            fixture.write("src/one.rs", "one");
            fixture.write("src/two.rs", "two");
            let asker =
                ScriptedAsker::new([Reply::RememberSourceCommand, Reply::Deny, Reply::Deny]);
            let engine = PermissionEngine::new(Decision::Ask, Some(asker.clone()));
            for (raw, allowed) in [
                ("cat src/one.rs", true),
                ("cat src/two.rs", true),
                ("cat Cargo.toml", false),
                ("rm src/one.rs", false),
            ] {
                let mut request = bash_req(raw);
                request.cwd = fixture.to_string_lossy().into_owned();
                assert_eq!(engine.check(&request).await, allowed, "{raw}");
            }
            assert_eq!(asker.calls(), 3);
            assert_eq!(
                serde_json::to_value(engine.export_grants()).unwrap(),
                serde_json::json!([
                    {"kind": "bash_command", "name": "cat", "glob": "src/**"}
                ])
            );
            assert_eq!(
                std::fs::read_to_string(fixture.join("src/one.rs")).unwrap(),
                "one"
            );
        }

        #[tokio::test]
        async fn mandatory_approval_is_fresh_in_either_rule_order_and_never_sticky() {
            for mandatory_first in [false, true] {
                let asker =
                    ScriptedAsker::new([Reply::Once, Reply::Deny, Reply::Cancel, Reply::Invalid]);
                let mut engine = PermissionEngine::new(Decision::Allow, Some(asker.clone()));
                for decision in if mandatory_first {
                    [Decision::RequireApproval, Decision::Allow]
                } else {
                    [Decision::Allow, Decision::RequireApproval]
                } {
                    engine.add(Arc::new(move |_| Some(decision)));
                }
                engine.set_mode(Mode::AutoAccept);
                for allowed in [true, false, false, false] {
                    assert_eq!(engine.check(&req_named("edit_file")).await, allowed);
                    assert!(engine.export_grants().is_empty());
                }
                assert_eq!(asker.calls(), 4);
                for options in asker.prompts.lock().unwrap().iter() {
                    assert_eq!(options.len(), 2);
                    assert!(options.iter().all(|option| option.grant.is_none()));
                    assert_eq!(options.iter().filter(|option| option.allow).count(), 1);
                }
            }
        }

        #[tokio::test]
        async fn hard_denial_beats_mandatory_approval_without_prompt_or_grant_changes() {
            for deny_first in [false, true] {
                let asker = ScriptedAsker::new([]);
                let mut engine = PermissionEngine::new(Decision::Allow, Some(asker.clone()));
                for decision in if deny_first {
                    [Decision::Deny, Decision::RequireApproval]
                } else {
                    [Decision::RequireApproval, Decision::Deny]
                } {
                    engine.add(Arc::new(move |_| Some(decision)));
                }
                engine.toggle_bypass();
                assert!(!engine.check(&req_named("edit_file")).await);
                assert_eq!(asker.calls(), 0);
                assert!(engine.export_grants().is_empty());
            }
        }

        #[tokio::test]
        async fn ordinary_rules_use_first_match_and_ignore_abstentions() {
            for (first, second, allowed, prompts) in [
                (Decision::Allow, Decision::Ask, true, 0),
                (Decision::Ask, Decision::Allow, false, 1),
            ] {
                let asker = ScriptedAsker::new([Reply::Deny]);
                let mut engine = PermissionEngine::new(Decision::Deny, Some(asker.clone()));
                engine.add(Arc::new(|_| None));
                engine.add(Arc::new(move |_| Some(first)));
                engine.add(Arc::new(move |_| Some(second)));
                assert_eq!(engine.check(&req_named("edit_file")).await, allowed);
                assert_eq!(asker.calls(), prompts);
                assert!(engine.export_grants().is_empty());
            }
        }

        #[tokio::test]
        async fn plan_mode_blocks_without_prompting_and_retains_existing_grants() {
            let asker = ScriptedAsker::new([]);
            let engine = PermissionEngine::new(Decision::Ask, Some(asker.clone()));
            engine.import_grants(vec![Grant::Tool {
                name: "new_mutator".into(),
            }]);
            let before = serde_json::to_value(engine.export_grants()).unwrap();
            engine.toggle_bypass();
            engine.set_mode(Mode::Plan);
            assert!(!engine.check(&req_named("new_mutator")).await);
            assert_eq!(asker.calls(), 0);
            assert_eq!(
                serde_json::to_value(engine.export_grants()).unwrap(),
                before
            );
        }
    }

    #[tokio::test]
    async fn mandatory_default_requires_ui_unless_yolo_is_enabled() {
        let engine = PermissionEngine::new(Decision::RequireApproval, None);
        engine.set_mode(Mode::AutoAccept);
        engine.import_grants(vec![Grant::Tool {
            name: "edit_file".into(),
        }]);
        assert!(!engine.check(&req_named("edit_file")).await);
        engine.toggle_bypass();
        assert!(engine.check(&req_named("edit_file")).await);
        engine.toggle_bypass();
        assert!(!engine.check(&req_named("edit_file")).await);
        assert_eq!(
            serde_json::to_value(engine.export_grants()).unwrap(),
            serde_json::json!([
                {"kind": "tool", "name": "edit_file"}
            ])
        );
    }

    #[test]
    fn grant_scope_requires_every_path_and_exact_command_identity() {
        let fixture = TestDir::new();
        fixture.write("src/lib.rs", "source");
        let grant = Grant::BashCommand {
            name: "cat".into(),
            glob: "src/**".into(),
        };
        for (raw, expected) in [
            ("/bin/cat src/lib.rs", true),
            ("cat src/lib.rs src/new.rs", true),
            ("cat src/lib.rs other/file", false),
            ("cat src/../outside", false),
            ("cat", false),
            ("cat -n", false),
            ("catfish src/lib.rs", false),
            ("cat src/lib.rs; cat src/new.rs", false),
            ("cat src/lib.rs | cat", false),
        ] {
            let mut request = bash_req(raw);
            request.cwd = fixture.to_string_lossy().into_owned();
            assert_eq!(grant_matches(&grant, &request), expected, "{raw}");
        }
        let mut request = bash_req("cat src/lib.rs");
        request.cwd = fixture.join("missing-root").to_string_lossy().into_owned();
        assert!(!grant_matches(&grant, &request));
    }

    #[cfg(unix)]
    #[test]
    fn scoped_grants_reject_symlink_escape_even_with_matching_spelling() {
        let fixture = TestDir::new();
        fixture.write("src/local", "local");
        fixture.write("outside/secret", "secret");
        std::os::unix::fs::symlink(fixture.join("outside"), fixture.join("src/link")).unwrap();
        let grant = Grant::BashCommand {
            name: "cat".into(),
            glob: "src/**".into(),
        };
        let mut request = bash_req("cat src/link/secret");
        request.cwd = fixture.to_string_lossy().into_owned();
        assert!(!grant_matches(&grant, &request));
        assert_eq!(
            std::fs::read_to_string(fixture.join("outside/secret")).unwrap(),
            "secret"
        );
    }

    #[test]
    fn glob_scopes_respect_segments_depth_and_literal_characters() {
        for (pattern, path, expected) in [
            ("", "", true),
            ("", "src", false),
            ("src/*", "src/lib.rs", true),
            ("src/*", "src/nested/lib.rs", false),
            ("src/**", "src/nested/lib.rs", true),
            ("src/**", "src-other/lib.rs", false),
            ("**/*.rs", "lib.rs", true),
            ("**/*.rs", "src/lib.rs", true),
            ("**/*.rs", "src/lib.rs.bak", false),
            ("src/?.rs", "src/a.rs", false),
            ("src/é*", "src/été.rs", true),
        ] {
            assert_eq!(glob_match(pattern, path), expected, "{pattern}: {path}");
        }
    }
}
