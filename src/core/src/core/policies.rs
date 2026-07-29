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
    "task",
    "spawn_agent",
    "send_message",
    "list_agents",
    "lsp",
    "job_status",
    "job_output",
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
    fn mutating_tools_are_not_auto_allowed() {
        let rule = allow_read_only();
        // These must fall through (None) so the engine can prompt/deny.
        for tool in ["write_file", "edit_file", "bash"] {
            assert_eq!(rule(&req(tool)), None, "{tool} must not be auto-allowed");
        }
    }
}
