use crate::contracts::*;
use crate::model::*;
use crate::store::decode;
use rusqlite::Connection;
use serde_json::Value;

pub(crate) fn permissions(db: &Connection, thread: &ThreadId) -> anyhow::Result<PermissionPolicy> {
    decode(db.query_row(
        "SELECT s.permissions FROM sessions s JOIN threads t ON t.session=s.id WHERE t.id=?1",
        [thread],
        |r| r.get(0),
    )?)
}

pub(crate) fn grant_path(workspace: &str, path: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path};
    let path = Path::new(path);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    #[cfg(windows)]
    if (path.has_root() && !path.is_absolute())
        || (matches!(path.components().next(), Some(Component::Prefix(_))) && !path.is_absolute())
    {
        return None;
    }
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let root = Path::new(workspace);
        if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
            return None;
        }
        root.join(path)
    };
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let mut normalized = std::path::PathBuf::new();
        let Component::Prefix(prefix) = full.components().next()? else {
            return None;
        };
        match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) if drive.is_ascii_alphabetic() => {
                normalized.push(format!(
                    "\\\\?\\{}:\\",
                    char::from(drive.to_ascii_uppercase())
                ));
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let mut unc = std::ffi::OsString::from("\\\\?\\UNC\\");
                unc.push(server);
                unc.push("\\");
                unc.push(share);
                unc.push("\\");
                normalized.push(unc);
            }
            _ => return None,
        }
        for component in full.components() {
            if let Component::Normal(name) = component {
                normalized.push(name);
            }
        }
        Some(normalized)
    }
    #[cfg(not(windows))]
    Some(
        full.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect(),
    )
}
pub(crate) fn evaluate_in_workspace(
    policy: &Policy,
    permissions: &PermissionPolicy,
    profile: &Profile,
    name: &str,
    input: &Value,
    capability: &ToolCapabilities,
    workspace: &str,
) -> RuleDecision {
    evaluate_with_fallback(
        policy,
        permissions,
        profile,
        ToolRequest {
            name,
            input,
            workspace,
        },
        capability,
        None,
    )
}

pub(crate) struct ToolRequest<'a> {
    pub name: &'a str,
    pub input: &'a Value,
    pub workspace: &'a str,
}

pub(crate) fn prepared_shell_permissions(
    permissions: &PermissionPolicy,
    request: ToolRequest<'_>,
    candidate: Option<&ShellCommandGrant>,
) -> anyhow::Result<Option<PermissionPolicy>> {
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    anyhow::ensure!(
        request.name == "bash" && candidate.well_formed(),
        "invalid prepared shell grant"
    );
    let command = request.input["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prepared shell command is missing"))?;
    let matches = |grant: &PermissionGrant| {
        grant.tool == request.name
            && grant.command.is_none()
            && grant.path_prefix.is_none()
            && grant.shell_command.as_ref() == Some(candidate)
    };
    if !permissions.grants.iter().any(matches) {
        return Ok(None);
    }
    let mut confirmed = permissions.clone();
    for grant in confirmed.grants.iter_mut().filter(|grant| matches(grant)) {
        // Only adapter-validated preparation can bind a program grant to this exact invocation.
        grant.shell_command = None;
        grant.command = Some(command.to_owned());
    }
    Ok(Some(confirmed))
}

pub(crate) fn evaluate_with_fallback(
    policy: &Policy,
    permissions: &PermissionPolicy,
    profile: &Profile,
    request: ToolRequest<'_>,
    capability: &ToolCapabilities,
    fallback: Option<RuleDecision>,
) -> RuleDecision {
    evaluate_with_target(
        policy,
        permissions,
        profile,
        request,
        capability,
        fallback,
        None,
    )
}

pub(crate) fn evaluate_with_target(
    policy: &Policy,
    permissions: &PermissionPolicy,
    profile: &Profile,
    request: ToolRequest<'_>,
    capability: &ToolCapabilities,
    fallback: Option<RuleDecision>,
    target: Option<&std::path::Path>,
) -> RuleDecision {
    let ToolRequest {
        name,
        input,
        workspace,
    } = request;
    let mut normalized = input.clone();
    let path = target.map(std::path::Path::to_path_buf).or_else(|| {
        input["path"]
            .as_str()
            .and_then(|path| grant_path(workspace, path))
    });
    if let Some(path) = path {
        let Some(path) = path.to_str() else {
            return RuleDecision::Deny;
        };
        if !normalized.is_object() {
            return RuleDecision::Deny;
        }
        normalized["path"] = Value::String(path.to_owned());
    }
    evaluate_inner(
        policy,
        permissions,
        profile,
        name,
        &normalized,
        capability,
        fallback,
    )
}

#[cfg(test)]
pub(crate) fn evaluate(
    policy: &Policy,
    permissions: &PermissionPolicy,
    profile: &Profile,
    name: &str,
    input: &Value,
    capability: &ToolCapabilities,
) -> RuleDecision {
    evaluate_inner(policy, permissions, profile, name, input, capability, None)
}

fn evaluate_inner(
    policy: &Policy,
    permissions: &PermissionPolicy,
    profile: &Profile,
    name: &str,
    input: &Value,
    capability: &ToolCapabilities,
    fallback: Option<RuleDecision>,
) -> RuleDecision {
    if policy.denied_tools.contains(name)
        || (profile.read_only
            && (capability.mutates_workspace
                || capability.executes_process
                || !capability.plan_safe))
        || ((policy.plan || permissions.mode == PermissionMode::Plan) && !capability.plan_safe)
    {
        return RuleDecision::Deny;
    }
    let rules: Vec<_> = permissions
        .rules
        .iter()
        .filter(|rule| rule.tool == name || rule.tool == "*")
        .collect();
    if rules.iter().any(|rule| rule.decision == RuleDecision::Deny) {
        return RuleDecision::Deny;
    }
    if policy.yolo {
        return RuleDecision::Allow;
    }
    if capability.requires_approval
        || rules
            .iter()
            .any(|rule| rule.decision == RuleDecision::RequireApproval)
    {
        return RuleDecision::RequireApproval;
    }
    if permissions.mode == PermissionMode::AutoAccept
        && matches!(
            name,
            "create_file" | "write_file" | "edit_file" | "multi_edit"
        )
        && capability.mutates_workspace
        && !capability.executes_process
    {
        return RuleDecision::Allow;
    }
    let granted = permissions.grants.iter().any(|grant| {
        grant.tool == name
            && grant.shell_command.is_none()
            && grant.command.as_ref().is_none_or(|command| {
                input.get("command").and_then(Value::as_str) == Some(command.as_str())
            })
            && grant.path_prefix.as_ref().is_none_or(|prefix| {
                grant_path("", prefix).is_some_and(|root| {
                    input
                        .get("path")
                        .and_then(Value::as_str)
                        .and_then(|path| grant_path("", path))
                        .is_some_and(|path| path.starts_with(root))
                })
            })
    });
    if granted {
        return RuleDecision::Allow;
    }
    if let Some(rule) = rules.first() {
        return rule.decision;
    }
    let known_read = !capability.mutates_workspace
        && !capability.executes_process
        && capability.plan_safe
        && (matches!(
            name,
            "read_file"
                | "project_instructions"
                | "list_dir"
                | "glob"
                | "grep"
                | "lsp"
                | "web_fetch"
                | "web_search"
        ) || internal_capabilities(name).is_some());
    if known_read {
        RuleDecision::Allow
    } else {
        fallback.unwrap_or(permissions.default)
    }
}

pub(crate) fn internal_capabilities(name: &str) -> Option<ToolCapabilities> {
    if matches!(
        name,
        "read_history" | "search_conversation" | "read_conversation"
    ) {
        return Some(ToolCapabilities {
            plan_safe: true,
            ..Default::default()
        });
    }
    matches!(
        name,
        "ask_user"
            | "enter_plan"
            | "exit_plan"
            | "todo_write"
            | "launch_agent"
            | "send_message"
            | "wait_message"
            | "wait_agent"
            | "task"
            | "explore"
            | "spawn_agent"
            | "list_agents"
            | "stop_agent"
            | "job_status"
            | "job_output"
            | "workflow"
    )
    .then(|| ToolCapabilities {
        mutates_state: true,
        plan_safe: true,
        ..Default::default()
    })
}

#[cfg(test)]
#[path = "path_authorization_tests.rs"]
mod path_authorization_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_program_grants_require_fresh_prepared_evidence_and_keep_guards() {
        let profile = Profile {
            model: "test".into(),
            instructions: String::new(),
            read_only: false,
            max_requests: 4,
            reasoning: bob_types::ReasoningEffort::Off,
        };
        let scope = ShellCommandGrant {
            dialect: "bash".into(),
            program: "git".into(),
        };
        let mut permissions = PermissionPolicy {
            rules: vec![PermissionRule {
                tool: "bash".into(),
                decision: RuleDecision::Ask,
            }],
            grants: vec![PermissionGrant {
                tool: "bash".into(),
                command: None,
                path_prefix: None,
                shell_command: Some(scope.clone()),
            }],
            ..Default::default()
        };
        let input = serde_json::json!({"command":"git status", "shell_command":{"dialect":"bash","program":"git"}});
        let request = || ToolRequest {
            name: "bash",
            input: &input,
            workspace: "",
        };
        let caps = ToolCapabilities {
            executes_process: true,
            mutates_workspace: true,
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                &Policy::default(),
                &permissions,
                &profile,
                "bash",
                &input,
                &caps
            ),
            RuleDecision::Ask
        );
        assert!(prepared_shell_permissions(&permissions, request(), None)
            .unwrap()
            .is_none());
        let other = ShellCommandGrant {
            program: "gitty".into(),
            ..scope.clone()
        };
        assert!(
            prepared_shell_permissions(&permissions, request(), Some(&other))
                .unwrap()
                .is_none()
        );
        for (rule, expected) in [
            (RuleDecision::Ask, RuleDecision::Allow),
            (RuleDecision::Deny, RuleDecision::Deny),
            (RuleDecision::RequireApproval, RuleDecision::RequireApproval),
        ] {
            permissions.rules[0].decision = rule;
            let before = serde_json::to_value(&permissions).unwrap();
            let confirmed = prepared_shell_permissions(&permissions, request(), Some(&scope))
                .unwrap()
                .unwrap();
            assert_eq!(
                evaluate(
                    &Policy::default(),
                    &confirmed,
                    &profile,
                    "bash",
                    &input,
                    &caps
                ),
                expected
            );
            assert_eq!(serde_json::to_value(&permissions).unwrap(), before);
        }
        permissions.rules.clear();
        let confirmed = prepared_shell_permissions(&permissions, request(), Some(&scope))
            .unwrap()
            .unwrap();
        let mandatory = ToolCapabilities {
            requires_approval: true,
            external_access: true,
            ..caps.clone()
        };
        assert_eq!(
            evaluate(
                &Policy::default(),
                &confirmed,
                &profile,
                "bash",
                &input,
                &mandatory
            ),
            RuleDecision::RequireApproval
        );
        assert_eq!(
            evaluate(
                &Policy::default(),
                &confirmed,
                &Profile {
                    read_only: true,
                    ..profile.clone()
                },
                "bash",
                &input,
                &caps
            ),
            RuleDecision::Deny
        );
        assert_eq!(
            evaluate(
                &Policy {
                    plan: true,
                    ..Default::default()
                },
                &confirmed,
                &profile,
                "bash",
                &input,
                &caps
            ),
            RuleDecision::Deny
        );
        let pwsh = ShellCommandGrant {
            dialect: "pwsh".into(),
            ..scope.clone()
        };
        assert!(prepared_shell_permissions(&permissions, request(), Some(&pwsh)).is_err());
        permissions.grants[0].command = Some("git status".into());
        assert!(
            prepared_shell_permissions(&permissions, request(), Some(&scope))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            evaluate(
                &Policy::default(),
                &permissions,
                &profile,
                "bash",
                &input,
                &caps
            ),
            RuleDecision::Ask
        );
        let legacy: PermissionGrant = serde_json::from_value(
            serde_json::json!({"tool":"bash","path_prefix":null,"command":"git status"}),
        )
        .unwrap();
        assert!(legacy.shell_command.is_none());
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("shell_command")
            .is_none());
    }

    #[test]
    fn auto_accept_is_limited_to_file_edits_and_preserves_safety_gates() {
        let mut profile = Profile {
            model: "test".into(),
            instructions: String::new(),
            read_only: false,
            max_requests: 4,
            reasoning: bob_types::ReasoningEffort::Off,
        };
        let mut permissions = PermissionPolicy {
            mode: PermissionMode::AutoAccept,
            ..Default::default()
        };
        for name in [
            "create_file",
            "write_file",
            "edit_file",
            "multi_edit",
            "rename_symbol",
            "code_action",
            "unclassified_mutator",
            "bash",
        ] {
            let mut capabilities = ToolCapabilities {
                mutates_workspace: true,
                executes_process: name == "bash",
                ..Default::default()
            };
            let expected = if matches!(
                name,
                "create_file" | "write_file" | "edit_file" | "multi_edit"
            ) {
                RuleDecision::Allow
            } else {
                RuleDecision::Ask
            };
            assert_eq!(
                evaluate(
                    &Policy::default(),
                    &permissions,
                    &profile,
                    name,
                    &Value::Null,
                    &capabilities
                ),
                expected,
                "{name}"
            );
            for decision in [RuleDecision::Deny, RuleDecision::RequireApproval] {
                permissions.rules = vec![PermissionRule {
                    tool: name.into(),
                    decision,
                }];
                assert_eq!(
                    evaluate(
                        &Policy::default(),
                        &permissions,
                        &profile,
                        name,
                        &Value::Null,
                        &capabilities
                    ),
                    decision
                );
            }
            permissions.rules.clear();
            capabilities.external_access = true;
            capabilities.requires_approval = true;
            assert_eq!(
                evaluate(
                    &Policy::default(),
                    &permissions,
                    &profile,
                    name,
                    &Value::Null,
                    &capabilities
                ),
                RuleDecision::RequireApproval
            );
            capabilities.external_access = false;
            capabilities.requires_approval = false;
            profile.read_only = true;
            assert_eq!(
                evaluate(
                    &Policy::default(),
                    &permissions,
                    &profile,
                    name,
                    &Value::Null,
                    &capabilities
                ),
                RuleDecision::Deny
            );
            profile.read_only = false;
            assert_eq!(
                evaluate(
                    &Policy {
                        plan: true,
                        ..Default::default()
                    },
                    &permissions,
                    &profile,
                    name,
                    &Value::Null,
                    &capabilities
                ),
                RuleDecision::Deny
            );
        }
    }

    #[test]
    fn archive_recall_is_read_only_and_respects_explicit_permissions_for_every_role() {
        let profile = Profile {
            model: "test".into(),
            instructions: String::new(),
            read_only: true,
            max_requests: 4,
            reasoning: bob_types::ReasoningEffort::Off,
        };
        for name in ["search_conversation", "read_conversation"] {
            let capability = internal_capabilities(name).unwrap();
            assert!(capability.plan_safe);
            assert!(!capability.mutates_state);
            assert!(!capability.mutates_workspace);
            assert!(!capability.executes_process);
            for role in ["root", "collaborator", "task", "workflow", "explore"] {
                assert!(crate::delegation::allowed(role, name));
            }
            for mode in [PermissionMode::Normal, PermissionMode::Plan] {
                let permissions = PermissionPolicy {
                    mode,
                    ..Default::default()
                };
                assert_eq!(
                    evaluate(
                        &Policy::default(),
                        &permissions,
                        &profile,
                        name,
                        &Value::Null,
                        &capability
                    ),
                    RuleDecision::Allow
                );
                for decision in [
                    RuleDecision::Deny,
                    RuleDecision::Ask,
                    RuleDecision::RequireApproval,
                ] {
                    let permissions = PermissionPolicy {
                        mode,
                        rules: vec![PermissionRule {
                            tool: name.into(),
                            decision,
                        }],
                        ..Default::default()
                    };
                    assert_eq!(
                        evaluate(
                            &Policy::default(),
                            &permissions,
                            &profile,
                            name,
                            &Value::Null,
                            &capability
                        ),
                        decision
                    );
                }
            }
        }
    }

    #[test]
    fn project_instructions_load_without_approval_by_default() {
        let mut profile = Profile {
            model: "test".into(),
            instructions: String::new(),
            read_only: false,
            max_requests: 4,
            reasoning: bob_types::ReasoningEffort::Off,
        };
        let capability = ToolCapabilities {
            plan_safe: true,
            ..Default::default()
        };
        for read_only in [false, true] {
            profile.read_only = read_only;
            for mode in [PermissionMode::Normal, PermissionMode::Plan] {
                let permissions = PermissionPolicy {
                    mode,
                    ..Default::default()
                };
                assert_eq!(
                    evaluate(
                        &Policy::default(),
                        &permissions,
                        &profile,
                        "project_instructions",
                        &serde_json::json!({"path":"."}),
                        &capability,
                    ),
                    RuleDecision::Allow,
                    "{mode:?}, read_only={read_only}",
                );
                assert_eq!(
                    evaluate(
                        &Policy::default(),
                        &permissions,
                        &profile,
                        "unclassified_read",
                        &Value::Null,
                        &capability,
                    ),
                    RuleDecision::Ask,
                );
            }
        }
    }

    #[test]
    fn automatic_project_instructions_preserve_permission_boundaries() {
        let profile = Profile {
            model: "test".into(),
            instructions: String::new(),
            read_only: false,
            max_requests: 4,
            reasoning: bob_types::ReasoningEffort::Off,
        };
        let capability = ToolCapabilities {
            plan_safe: true,
            ..Default::default()
        };
        for tool in ["project_instructions", "*"] {
            for decision in [
                RuleDecision::Deny,
                RuleDecision::Ask,
                RuleDecision::RequireApproval,
            ] {
                let permissions = PermissionPolicy {
                    rules: vec![PermissionRule {
                        tool: tool.into(),
                        decision,
                    }],
                    ..Default::default()
                };
                assert_eq!(
                    evaluate(
                        &Policy::default(),
                        &permissions,
                        &profile,
                        "project_instructions",
                        &Value::Null,
                        &capability,
                    ),
                    decision,
                );
            }
        }
        let policy = Policy {
            denied_tools: ["project_instructions".into()].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(
            evaluate(
                &policy,
                &PermissionPolicy::default(),
                &profile,
                "project_instructions",
                &Value::Null,
                &capability,
            ),
            RuleDecision::Deny,
        );
        let external = ToolCapabilities {
            external_access: true,
            requires_approval: true,
            ..capability
        };
        assert_eq!(
            evaluate(
                &Policy::default(),
                &PermissionPolicy::default(),
                &profile,
                "project_instructions",
                &Value::Null,
                &external,
            ),
            RuleDecision::RequireApproval,
        );
    }

    #[test]
    fn yolo_never_overrides_denials_plan_or_child_restrictions() {
        let mut policy = Policy {
            yolo: true,
            ..Default::default()
        };
        let mut permissions = PermissionPolicy::default();
        let mut profile = Profile {
            model: "test".into(),
            instructions: String::new(),
            read_only: false,
            max_requests: 200,
            reasoning: bob_types::ReasoningEffort::Off,
        };
        let capability = ToolCapabilities {
            mutates_workspace: true,
            requires_approval: true,
            ..Default::default()
        };
        permissions.rules.push(PermissionRule {
            tool: "edit".into(),
            decision: RuleDecision::RequireApproval,
        });
        assert_eq!(
            evaluate(
                &policy,
                &permissions,
                &profile,
                "edit",
                &Value::Null,
                &capability
            ),
            RuleDecision::Allow
        );
        permissions.rules.push(PermissionRule {
            tool: "edit".into(),
            decision: RuleDecision::Deny,
        });
        assert_eq!(
            evaluate(
                &policy,
                &permissions,
                &profile,
                "edit",
                &Value::Null,
                &capability
            ),
            RuleDecision::Deny
        );
        permissions.rules.clear();
        policy.plan = true;
        assert_eq!(
            evaluate(
                &policy,
                &permissions,
                &profile,
                "edit",
                &Value::Null,
                &capability
            ),
            RuleDecision::Deny
        );
        policy.plan = false;
        profile.read_only = true;
        assert_eq!(
            evaluate(
                &policy,
                &permissions,
                &profile,
                "edit",
                &Value::Null,
                &capability
            ),
            RuleDecision::Deny
        );
        let memory = ToolCapabilities {
            plan_safe: true,
            ..Default::default()
        };
        policy.plan = true;
        assert_eq!(
            evaluate(
                &policy,
                &permissions,
                &profile,
                "memory",
                &Value::Null,
                &memory
            ),
            RuleDecision::Allow
        );
    }
}
