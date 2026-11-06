use super::*;
use crate::approvals::choices_with_target;
use serde_json::json;
use std::path::PathBuf;

fn root() -> PathBuf {
    #[cfg(windows)]
    let root = PathBuf::from(r"\\?\C:\Workspace");
    #[cfg(not(windows))]
    let root = PathBuf::from("/workspace");
    root
}

fn prepared(input: Value) -> PreparedTool {
    PreparedTool {
        invocation: Invocation {
            operation_id: OperationId::new(),
            run_id: RunId::new(),
            thread_id: ThreadId::new(),
            workspace: root().join("launch-alias").to_str().unwrap().to_owned(),
            name: "write_file".into(),
            input,
        },
        specification: json!({}),
        preview: String::new(),
        presentation: None,
    }
}

#[test]
fn primary_target_drives_policy_and_exact_grant_without_mutating_invocation() {
    let prepared = prepared(json!({"path":"missing/new.txt","content":"approved"}));
    let original = serde_json::to_value(&prepared.invocation).unwrap();
    let target = root().join("physical/missing/new.txt");
    let options = choices_with_target(&prepared, true, Some(&target));
    let grant = options
        .iter()
        .find_map(|option| option.grant.clone())
        .unwrap();
    assert_eq!(grant.path_prefix.as_deref(), target.to_str());
    assert_ne!(
        grant.path_prefix.as_deref(),
        target.parent().unwrap().to_str()
    );
    assert!(choices_with_target(&prepared, false, Some(&target))
        .iter()
        .all(|option| option.grant.is_none()));
    let profile = Profile {
        model: "test".into(),
        instructions: String::new(),
        read_only: false,
        max_requests: 1,
        reasoning: bob_types::ReasoningEffort::Off,
    };
    let policy = Policy::default();
    let permissions = PermissionPolicy {
        grants: vec![grant],
        ..Default::default()
    };
    let caps = ToolCapabilities {
        mutates_workspace: true,
        ..Default::default()
    };
    let evaluate = |target: &std::path::Path| {
        evaluate_with_target(
            &policy,
            &permissions,
            &profile,
            ToolRequest {
                name: &prepared.invocation.name,
                input: &prepared.invocation.input,
                workspace: &prepared.invocation.workspace,
            },
            &caps,
            None,
            Some(target),
        )
    };
    assert_eq!(evaluate(&target), RuleDecision::Allow);
    assert_eq!(
        evaluate(&target.with_file_name("sibling.txt")),
        RuleDecision::Ask
    );
    assert_eq!(
        evaluate(&target.with_file_name("New.txt")),
        RuleDecision::Ask
    );
    assert_eq!(
        serde_json::to_value(&prepared.invocation).unwrap(),
        original
    );
}

#[test]
fn default_directory_primary_target_does_not_offer_tool_wide_grant() {
    let mut prepared = prepared(json!({}));
    prepared.invocation.name = "list_dir".into();
    let target = root();
    let options = choices_with_target(&prepared, true, Some(&target));
    let grant = options
        .iter()
        .find_map(|option| option.grant.as_ref())
        .unwrap();
    assert_eq!(grant.tool, "list_dir");
    assert_eq!(grant.path_prefix.as_deref(), target.to_str());
    assert!(grant.command.is_none());
}

#[cfg(windows)]
#[test]
fn windows_permission_paths_normalize_only_supported_prefixes() {
    for (dos, verbatim) in [
        (r"C:\Workspace\日本語.txt", r"\\?\C:\Workspace\日本語.txt"),
        (r"c:\Workspace\日本語.txt", r"\\?\C:\Workspace\日本語.txt"),
        (
            r"\\server\share\日本語.txt",
            r"\\?\UNC\server\share\日本語.txt",
        ),
    ] {
        assert_eq!(grant_path("", dos), grant_path("", verbatim));
    }
    assert_ne!(
        grant_path("", r"C:\Workspace\Case"),
        grant_path("", r"C:\Workspace\case")
    );
    for invalid in [
        r"C:relative",
        r"\root-relative",
        r"\\.\C:\file",
        r"\\?\GLOBALROOT\Device\x",
        r"C:\a\..\b",
    ] {
        assert!(grant_path(r"C:\Workspace", invalid).is_none(), "{invalid}");
    }
}
