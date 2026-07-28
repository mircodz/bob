//! Permission model. Every tool call is gated through a `PermissionEngine`
//! before it executes. Policies are ordered rules; the first match wins. When a
//! decision is "ask", the engine builds a set of `PermissionOption`s and
//! delegates to an `Asker` (the UI) to pick one. Picking an "always" option
//! registers a session grant so the same class of call is auto-allowed after.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

pub struct PermissionRequest {
    pub tool: String,
    pub input: Value,
    pub cwd: String,
    /// For bash: the parsed command structure, so rules can inspect it.
    pub bash: Option<ParsedCommand>,
    /// A side-effect-free preview of the action (e.g. an edit's diff), rendered
    /// in the prompt so the user sees what they're approving.
    pub preview: Option<String>,
}

/// A single choice presented to the user when a decision is "ask". The `grant`
/// is what gets remembered (for the session) if this option is chosen; None for
/// one-shot allow/deny.
#[derive(Clone, Debug)]
pub struct PermissionOption {
    /// Human-readable label, e.g. "Allow always: rm src/**".
    pub label: String,
    /// Whether choosing this permits the call.
    pub allow: bool,
    /// A sticky rule to remember for the rest of the session.
    pub grant: Option<Grant>,
}

/// A session-scoped auto-allow rule. Two flavors: a whole tool, or a bash
/// command scoped to a path glob. Persisted with the session so "always allow"
/// survives a `--resume`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grant {
    /// Allow every future call to this tool (e.g. "always allow write_file").
    Tool(String),
    /// Allow a bash command name whose path-like args match `glob`.
    /// glob == "**" means "any args".
    BashCommand { name: String, glob: String },
}

/// The UI implements this: given the request + options, return the chosen index.
/// Returning None (or an out-of-range index) is treated as deny.
#[async_trait::async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, req: &PermissionRequest, options: &[PermissionOption]) -> Option<usize>;
}

/// A rule returns a decision, or None to abstain (let the next rule decide).
pub type Rule = Arc<dyn Fn(&PermissionRequest) -> Option<Decision> + Send + Sync>;

/// Interaction mode, cycled by the user (Shift+Tab in the TUI). Governs how much
/// bob may do without asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Normal: risky actions prompt per the rules.
    Normal,
    /// Auto-accept edits: file edits/writes are auto-allowed; bash still gated.
    AutoAccept,
    /// Plan: read-only. ALL mutating tools are blocked so bob researches and
    /// proposes a plan without changing anything.
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
    /// Cycle to the next mode (Normal → AutoAccept → Plan → Normal).
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

/// Tools that MUTATE state (files, shell). Used for mode gating.
pub fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "edit_file" | "multi_edit" | "bash"
    )
}

/// Tools that edit FILES (auto-accepted in AutoAccept mode).
fn is_file_edit_tool(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "multi_edit")
}

pub struct PermissionEngine {
    default: Decision,
    rules: Vec<Rule>,
    asker: Option<Arc<dyn Asker>>,
    /// Session grants accumulated via "always allow" choices.
    grants: Mutex<Vec<Grant>>,
    /// Current interaction mode (Normal/AutoAccept/Plan), cycled by the UI.
    mode: std::sync::atomic::AtomicU8,
}

impl PermissionEngine {
    pub fn new(default: Decision, asker: Option<Arc<dyn Asker>>) -> Self {
        PermissionEngine {
            default,
            rules: Vec::new(),
            asker,
            grants: Mutex::new(Vec::new()),
            mode: std::sync::atomic::AtomicU8::new(Mode::Normal.as_u8()),
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode.as_u8(), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn add(&mut self, rule: Rule) -> &mut Self {
        self.rules.push(rule);
        self
    }

    pub fn set_asker(&mut self, asker: Arc<dyn Asker>) {
        self.asker = Some(asker);
    }

    /// Resolve to true if the tool call is permitted.
    pub async fn check(&self, req: &PermissionRequest) -> bool {
        let mode = self.mode();

        // Plan mode: block every mutating tool outright (read-only research).
        // The `exit_plan` tool is exempt — it's how bob leaves plan mode.
        if mode == Mode::Plan && is_mutating_tool(&req.tool) {
            return false;
        }
        // Auto-accept mode: file edits go through without asking.
        if mode == Mode::AutoAccept && is_file_edit_tool(&req.tool) {
            return true;
        }

        // Session grants win next — they're the "always allow" the user chose.
        if self.grant_allows(req) {
            return true;
        }

        let mut decision = self.default;
        for rule in &self.rules {
            if let Some(d) = rule(req) {
                decision = d;
                break;
            }
        }
        match decision {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask => {
                let asker = match &self.asker {
                    Some(a) => a,
                    None => return false, // no UI ⇒ fail closed
                };
                let options = build_options(req);
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

    /// Snapshot the current session grants (for persisting into the session).
    pub fn export_grants(&self) -> Vec<Grant> {
        self.grants.lock().unwrap().clone()
    }

    /// Seed grants from a restored session.
    pub fn import_grants(&self, grants: Vec<Grant>) {
        *self.grants.lock().unwrap() = grants;
    }
}

/// Does a stored grant cover this request?
fn grant_matches(grant: &Grant, req: &PermissionRequest) -> bool {
    match grant {
        Grant::Tool(name) => &req.tool == name,
        Grant::BashCommand { name, glob } => {
            let Some(bash) = &req.bash else { return false };
            // Only ever match simple, single commands (see build_options).
            if !is_simple(bash) {
                return false;
            }
            let Some(argv) = bash.commands.first() else { return false };
            let Some(cmd) = argv.first() else { return false };
            if base_name(cmd) != name {
                return false;
            }
            if glob == "**" {
                return true;
            }
            // Every path-like arg must match the granted glob.
            let paths = path_args(argv);
            !paths.is_empty() && paths.iter().all(|p| glob_match(glob, p))
        }
    }
}

/// A command is "simple" (safe to scope) when it's a single command with no
/// operators, pipes, or redirects — so we can reason about its args honestly.
fn is_simple(bash: &ParsedCommand) -> bool {
    bash.commands.len() == 1 && bash.operators.is_empty() && !bash.pipes_to_shell
}

/// Positional, path-like args (skip flags like -rf, and env/opts with '=').
fn path_args(argv: &[String]) -> Vec<String> {
    argv.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-') && !a.contains('='))
        .cloned()
        .collect()
}

/// Build the choice list for an "ask" decision. Always includes allow-once and
/// deny; for simple bash commands, adds scoped + broad "always" grants.
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

                    // Scoped grant: derive a glob from the command's paths.
                    if let Some(glob) = scope_glob(&paths) {
                        opts.push(PermissionOption {
                            label: format!("Always allow `{}` in {}", name, glob),
                            allow: true,
                            grant: Some(Grant::BashCommand { name: name.clone(), glob }),
                        });
                    }
                    // Broad grant: any invocation of this command.
                    opts.push(PermissionOption {
                        label: format!("Always allow `{}` (any args)", name),
                        allow: true,
                        grant: Some(Grant::BashCommand { name, glob: "**".to_string() }),
                    });
                }
            }
        }
    } else {
        // Non-bash tool: offer an always-allow-this-tool grant.
        opts.push(PermissionOption {
            label: format!("Always allow {}", req.tool),
            allow: true,
            grant: Some(Grant::Tool(req.tool.clone())),
        });
    }

    opts.push(PermissionOption {
        label: "Deny".to_string(),
        allow: false,
        grant: None,
    });
    opts
}

/// Turn a command's path args into a directory-scoped glob, e.g.
/// ["src/a.rs","src/b.rs"] → "src/**". Returns None when there are no paths or
/// a sensible common scope can't be derived (caller still offers the broad "**").
fn scope_glob(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let first = paths[0].trim_start_matches("./");
    // Bare filename (no directory) → no meaningful scope beyond the broad "**".
    if !first.contains('/') {
        return None;
    }
    let top = first.split('/').next().unwrap_or(first);
    if top.is_empty() {
        return None;
    }
    // Only offer the scoped glob if *all* paths share that top segment.
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

/// Minimal glob matcher supporting `*` (within a segment) and `**` (any depth).
/// Enough for the path scopes we generate; not a full fnmatch.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn helper(p: &[u8], s: &[u8]) -> bool {
        // Handle `**` (matches across '/').
        if p.starts_with(b"**") {
            // Consume optional following '/'.
            let rest = if p.len() > 2 && p[2] == b'/' { &p[3..] } else { &p[2..] };
            if rest.is_empty() {
                return true;
            }
            // Try to match `rest` at every position of s.
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
                // `*` matches zero+ chars except '/'.
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

/* ------------------------------------------------------------------ */
/* Bash command parsing — lets shell policies reason about structure.  */
/* ------------------------------------------------------------------ */

#[derive(Clone, Debug, Default)]
pub struct ParsedCommand {
    /// Each simple command as an argv array. `rm -rf /` → ["rm","-rf","/"].
    pub commands: Vec<Vec<String>>,
    /// Raw operators between/around commands: | && || ; > >> < &
    pub operators: Vec<String>,
    /// True if any pipe feeds into a shell interpreter (curl | sh style).
    pub pipes_to_shell: bool,
    pub raw: String,
}

const SHELL_INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "python", "python3", "node", "ruby", "perl",
];

const OPERATORS: &[&str] = &["&&", "||", ";", "|", ">>", ">", "<", "&"];

fn base_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

pub fn parse_bash(raw: &str) -> ParsedCommand {
    let tokens = tokenize(raw);
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut operators: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for (value, is_op) in tokens {
        if is_op {
            if !current.is_empty() {
                commands.push(std::mem::take(&mut current));
            }
            operators.push(value);
        } else {
            current.push(value);
        }
    }
    if !current.is_empty() {
        commands.push(current);
    }

    let mut pipes_to_shell = false;
    for (i, cmd) in commands.iter().enumerate() {
        if i > 0 {
            if let Some(first) = cmd.first() {
                if SHELL_INTERPRETERS.contains(&base_name(first)) {
                    pipes_to_shell = true;
                }
            }
        }
    }

    ParsedCommand {
        commands,
        operators,
        pipes_to_shell,
        raw: raw.to_string(),
    }
}

fn tokenize(input: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens: Vec<(String, bool)> = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;

    macro_rules! push_word {
        () => {
            if !buf.is_empty() {
                tokens.push((std::mem::take(&mut buf), false));
            }
        };
    }

    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                buf.push(ch);
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == ' ' || ch == '\t' || ch == '\n' {
            push_word!();
            i += 1;
            continue;
        }
        // Try to match a multi-char operator first.
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let matched = OPERATORS.iter().find(|o| {
            if o.len() == 2 {
                two == **o
            } else {
                ch.to_string() == **o
            }
        });
        if let Some(op) = matched {
            push_word!();
            tokens.push((op.to_string(), true));
            i += op.len();
            continue;
        }
        buf.push(ch);
        i += 1;
    }
    push_word!();
    tokens
}
