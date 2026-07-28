//! A starter set of sensible policy rules. First match wins in the engine.

use crate::core::permissions::{Decision, PermissionRequest, Rule};
use std::collections::HashSet;
use std::sync::Arc;

// Tools that never mutate anything themselves. `task` spawns subagents, but
// every tool call *inside* a subagent is still gated by this same engine, so
// approving the spawn grants nothing on its own — auto-allow it to avoid a
// pointless prompt before the real, individually-gated work.
const READ_ONLY: &[&str] = &[
    "read_file", "list_dir", "glob", "grep", "todo_write", "task",
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

/// Block obviously destructive shell patterns outright.
pub fn deny_dangerous_bash() -> Rule {
    Arc::new(|req: &PermissionRequest| {
        if req.tool != "bash" {
            return None;
        }
        let bash = req.bash.as_ref()?;

        // curl … | sh  and friends.
        if bash.pipes_to_shell {
            return Some(Decision::Deny);
        }

        for argv in &bash.commands {
            let cmd = match argv.first() {
                Some(c) => c,
                None => continue,
            };
            let name = cmd.rsplit('/').next().unwrap_or(cmd);
            let args = &argv[1..];

            // rm -rf on root-ish paths.
            if name == "rm"
                && args.iter().any(|a| a.contains('r'))
                && args.iter().any(|a| a == "/" || a == "/*" || a == "~")
            {
                return Some(Decision::Deny);
            }
            if name == "dd" && args.iter().any(|a| a.starts_with("of=/dev/")) {
                return Some(Decision::Deny);
            }
            if name == "mkfs" || name == "shutdown" || name == "reboot" {
                return Some(Decision::Deny);
            }
        }
        None
    })
}

/// Allow a curated allowlist of harmless shell commands without asking.
pub fn allow_bash_commands(names: Vec<String>) -> Rule {
    let set: HashSet<String> = names.into_iter().collect();
    Arc::new(move |req: &PermissionRequest| {
        if req.tool != "bash" {
            return None;
        }
        let bash = req.bash.as_ref()?;
        let all_safe = bash.commands.iter().all(|argv| {
            argv.first()
                .map(|c| set.contains(c.rsplit('/').next().unwrap_or(c)))
                .unwrap_or(false)
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
