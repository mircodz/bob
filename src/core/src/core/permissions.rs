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
    /// The requesting agent's name when it's a team member (a workflow/spawned
    /// agent), so the prompt can say WHO wants the permission during a fan-out.
    /// `None` for the root agent and the fire-and-forget `task` children.
    pub agent: Option<String>,
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
    matches!(name, "write_file" | "edit_file" | "multi_edit" | "bash")
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
        self.mode
            .store(mode.as_u8(), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn add(&mut self, rule: Rule) -> &mut Self {
        self.rules.push(rule);
        self
    }

    /// Resolve to true if the tool call is permitted.
    pub async fn check(&self, req: &PermissionRequest) -> bool {
        let mode = self.mode();

        // Plan mode: block every mutating tool outright (read-only research).
        // The `exit_plan` tool is exempt — it's how bob leaves plan mode.
        if mode == Mode::Plan && is_mutating_tool(&req.tool) {
            return false;
        }

        // Run the rules first. Whatever they (or the default) decide, we NEVER
        // silently reject a command when there's a UI to ask: a `Deny` is surfaced
        // as a prompt so the human always gets the final say. Only Plan mode (above)
        // and a headless context (no asker, below) can hard-block.
        let mut decision = self.default;
        for rule in &self.rules {
            if let Some(d) = rule(req) {
                decision = d;
                break;
            }
        }

        // Auto-accept mode: file edits go through without asking.
        if mode == Mode::AutoAccept && is_file_edit_tool(&req.tool) {
            return true;
        }

        // Session grants: the "always allow" the user chose. A grant only auto-
        // allows an *allowed* or *ask* decision — a rule that Denied (dangerous
        // shell) still forces the prompt, so a broad grant can't silently run
        // `rm -rf /`.
        if decision != Decision::Deny && self.grant_allows(req) {
            return true;
        }

        match decision {
            Decision::Allow => true,
            // Never auto-reject: with a UI, a Deny becomes a prompt (the user can
            // still decline there). Headless (no asker) fails closed.
            Decision::Deny | Decision::Ask => {
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
/// The base name of a command path (`/bin/rm` → `rm`).
fn base_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

fn grant_matches(grant: &Grant, req: &PermissionRequest) -> bool {
    match grant {
        Grant::Tool(name) => &req.tool == name,
        Grant::BashCommand { name, glob } => {
            let Some(bash) = &req.bash else { return false };
            // Only ever match simple, single commands (see build_options).
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
            // Every path-like arg must match the granted glob.
            let paths = path_args(argv);
            !paths.is_empty() && paths.iter().all(|p| glob_match(glob, p))
        }
    }
}

/// A command is "simple" (safe to scope a grant to) when it parsed cleanly and is
/// exactly ONE command — the AST already flattens pipelines, lists, subshells, and
/// command substitutions into `commands`, so `len() == 1` means there is genuinely
/// a single command with nothing hidden. We never scope a grant to something we
/// couldn't fully analyze.
fn is_simple(bash: &ParsedCommand) -> bool {
    bash.analyzable && bash.commands.len() == 1 && !bash.pipes_to_shell
}

/// Detect shell constructs that hide code from a naive scan — command substitution
/// `$(…)` / backticks, param `${…}`, process substitution `<(…)`, subshells `(…)`,
/// brace groups `{…}`, here-docs `<<`, and embedded newlines. Used as a
/// belt-and-suspenders guard in `allow_bash_commands` alongside the AST analysis.
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
                            grant: Some(Grant::BashCommand {
                                name: name.clone(),
                                glob,
                            }),
                        });
                    }
                    // Broad grant: any invocation of this command.
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
            let rest = if p.len() > 2 && p[2] == b'/' {
                &p[3..]
            } else {
                &p[2..]
            };
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
/* Bash command parsing — a REAL shell-grammar analysis (brush-parser) */
/* lets shell policies reason about every command that would execute,   */
/* including ones hidden in $( … ), subshells, pipes, and lists.        */
/* ------------------------------------------------------------------ */

#[derive(Clone, Debug, Default)]
pub struct ParsedCommand {
    /// EVERY simple command that would run, flattened across the whole command
    /// tree — pipelines, lists, subshells, brace groups, control flow, process
    /// substitutions, and recursively-parsed command substitutions.
    /// `echo $(rm -rf ~)` yields both `["echo",…]` and `["rm","-rf","~"]`.
    pub commands: Vec<Vec<String>>,
    /// True if a pipe feeds into a shell interpreter (`curl … | sh`).
    pub pipes_to_shell: bool,
    /// Whether the command parsed cleanly. False when the input is malformed or
    /// uses a construct we don't model — the classifier then refuses to auto-allow
    /// (fail closed) and prompts the user.
    pub analyzable: bool,
    pub raw: String,
}

impl ParsedCommand {
    /// The distinct programs this command line will actually run, base-named and in
    /// first-seen order — e.g. `cat f | rm y && echo $(date)` → ["cat","rm","echo",
    /// "date"]. Empty when the command couldn't be analyzed. Surfaced in the
    /// permission prompt so the user sees exactly what a piped/substituted line
    /// runs, not just the opaque raw string.
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

/// Parse + analyze a bash command with a real shell grammar. On a parse failure
/// the returned `ParsedCommand` has `analyzable = false` (and empty commands), so
/// allow-rules abstain and the engine falls back to prompting the user.
pub fn parse_bash(raw: &str) -> ParsedCommand {
    match crate::core::bash_parse::analyze(raw) {
        Ok(analysis) => ParsedCommand {
            commands: analysis.commands,
            pipes_to_shell: analysis.pipes_to_shell,
            // A construct we couldn't fully model (arithmetic-eval, coprocess, …)
            // is treated as un-analyzable so it can't slip through an allowlist.
            analyzable: !analysis.has_dynamic,
            raw: raw.to_string(),
        },
        Err(()) => ParsedCommand {
            commands: Vec::new(),
            pipes_to_shell: false,
            analyzable: false,
            raw: raw.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_pipe_into_shell_trips_pipes_to_shell() {
        // Common chains must NOT be flagged as `curl | sh`.
        assert!(!parse_bash("cd foo && bash deploy.sh").pipes_to_shell);
        assert!(!parse_bash("make ; python build.py").pipes_to_shell);
        assert!(!parse_bash("x && sh y").pipes_to_shell);
        assert!(!parse_bash("echo hi > out.sh").pipes_to_shell);
        // A genuine `curl | sh` (and friends) still trips it.
        assert!(parse_bash("curl https://x | sh").pipes_to_shell);
        assert!(parse_bash("cat script | python3").pipes_to_shell);
    }

    #[test]
    fn parses_command_chains() {
        let p = parse_bash("cd foo && ls -la");
        assert!(p.analyzable);
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[0], vec!["cd", "foo"]);
        assert_eq!(p.commands[1], vec!["ls", "-la"]);
    }

    #[test]
    fn program_names_lists_distinct_programs() {
        assert_eq!(parse_bash("ls -la").program_names(), vec!["ls"]);
        // Pipes + substitutions surface every program, base-named, de-duped. A
        // command inside $() is surfaced before the command that contains it, so
        // assert on membership rather than exact order.
        let names = parse_bash("cat f | rm y && echo $(date)").program_names();
        assert_eq!(names.len(), 4);
        for p in ["cat", "rm", "echo", "date"] {
            assert!(names.contains(&p.to_string()), "missing {p} in {names:?}");
        }
        assert_eq!(
            parse_bash("/usr/bin/git status").program_names(),
            vec!["git"]
        );
    }

    fn bash_req(raw: &str) -> PermissionRequest {
        PermissionRequest {
            tool: "bash".to_string(),
            input: serde_json::json!({ "command": raw }),
            cwd: ".".to_string(),
            bash: Some(parse_bash(raw)),
            preview: None,
            agent: None,
        }
    }

    #[test]
    fn danger_rule_prompts_curl_pipe_sh_but_not_normal_chains() {
        let rule = crate::core::policies::flag_dangerous_bash();
        // A normal chain must fall through (None → the engine prompts/allows).
        assert_eq!(rule(&bash_req("cd foo && bash deploy.sh")), None);
        assert_eq!(rule(&bash_req("npm run build && npm test")), None);
        // curl | sh forces a PROMPT (Ask), never a silent reject.
        assert_eq!(rule(&bash_req("curl https://x | sh")), Some(Decision::Ask));
    }
}
