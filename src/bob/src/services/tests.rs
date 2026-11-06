use super::*;
use bob_core::core::memory::InMemoryMemories;
use bob_kernel::{OperationId, RunId, ThreadId};
use serde_json::json;

async fn fixture() -> Services {
    Services::create(
        &BobConfig::default(),
        &std::env::current_dir().unwrap(),
        Arc::new(InMemoryMemories::default()),
    )
    .await
    .unwrap()
}

fn invocation(services: &Services, name: &str, input: Value) -> Invocation {
    Invocation {
        operation_id: OperationId("fixture-operation".into()),
        run_id: RunId("fixture-run".into()),
        thread_id: ThreadId("fixture-thread".into()),
        workspace: services.workspace.to_string_lossy().into(),
        name: name.into(),
        input,
    }
}

#[tokio::test]
async fn integration_catalog_is_explicit_and_memory_is_not_a_workspace_edit() {
    let services = fixture().await;
    let descriptors = services.descriptors();
    let names: Vec<_> = descriptors
        .iter()
        .map(|descriptor| descriptor.spec.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "web_fetch",
            "web_search",
            "memory",
            "lsp",
            "rename_symbol",
            "code_action"
        ]
    );
    assert!(services.mcp_statuses().is_empty());
    assert!(services.lsp_statuses().is_empty());
    let capabilities = services.tool_capabilities("memory");
    assert!(capabilities.plan_safe && !capabilities.requires_approval);
    assert!(!capabilities.mutates_workspace && !capabilities.executes_process);
    let list = services
        .prepare(invocation(&services, "memory", json!({"action":"list"})))
        .await
        .unwrap();
    assert!(
        !services
            .prepared_capabilities(&list)
            .unwrap()
            .requires_approval
    );
    let action = services
        .prepare(invocation(
            &services,
            "code_action",
            json!({"filePath":"fixture.rs","line":1,"character":1}),
        ))
        .await
        .unwrap();
    assert!(services.prepared_capabilities(&action).unwrap().plan_safe);
    assert!(
        !services
            .prepared_capabilities(&action)
            .unwrap()
            .mutates_workspace
    );
}

#[tokio::test]
async fn preparation_is_pure_and_mutated_inputs_fail_before_execution() {
    let services = fixture().await;
    let request = invocation(&services, "memory", json!({"content":"fixture preference"}));
    let prepared = services.prepare(request).await.unwrap();
    assert!(services
        .memory_entries(&services.project_scope(), 0)
        .unwrap()
        .is_empty());
    let mut changed = prepared.clone();
    changed.invocation.input["content"] = json!("changed preference");
    assert!(services.prepared_capabilities(&changed).is_err());
    let (_sender, cancel) = watch::channel(false);
    assert!(services.execute(changed, cancel.clone()).await.is_err());
    let mut changed_plan = prepared.clone();
    changed_plan.specification["action"]["request"]["content"] = json!("changed preference");
    assert!(services
        .execute(changed_plan, cancel.clone())
        .await
        .is_err());
    assert!(services
        .memory_entries(&services.project_scope(), 0)
        .unwrap()
        .is_empty());
    let result = services.execute(prepared, cancel).await.unwrap();
    assert!(result.text.contains("fixture preference"));
    assert_eq!(
        services
            .memory_entries(&services.project_scope(), 0)
            .unwrap()[0]
            .content,
        "fixture preference"
    );
    assert!(services
        .memory_entries(&MemoryScope::Project("another project".into()), 0)
        .is_err());
}

#[tokio::test]
async fn network_requests_are_validated_before_io_and_precancellation_is_immediate() {
    let services = fixture().await;
    assert!(services
        .prepare(invocation(
            &services,
            "web_fetch",
            json!({"url":"file:///private"})
        ))
        .await
        .is_err());
    assert!(services
        .prepare(invocation(&services, "web_search", json!({"query":" "})))
        .await
        .is_err());
    let prepared = services
        .prepare(invocation(
            &services,
            "web_fetch",
            json!({"url":"https://example.invalid"}),
        ))
        .await
        .unwrap();
    let (_sender, cancel) = watch::channel(true);
    assert!(
        services
            .execute(prepared, cancel)
            .await
            .unwrap()
            .interrupted
    );
}

#[tokio::test]
async fn lsp_query_spec_cannot_substitute_another_input() {
    let services = fixture().await;
    let mut prepared = services
        .prepare(invocation(
            &services,
            "lsp",
            json!({"operation":"workspace_symbols","query":"original"}),
        ))
        .await
        .unwrap();
    prepared.specification["action"]["request"]["input"]["query"] = json!("substituted");
    let (_sender, cancel) = watch::channel(false);
    let error = services.execute(prepared, cancel).await.unwrap_err();
    assert!(error.to_string().contains("changed"));
}

#[tokio::test]
async fn lsp_write_action_binding_rejects_content_and_identity_substitution() {
    let directory = std::env::temp_dir().join(format!("bob-bound-lsp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let directory = std::fs::canonicalize(directory).unwrap();
    let path = directory.join("a.rs");
    std::fs::write(&path, "old\n").unwrap();
    let second = directory.join("b.rs");
    std::fs::write(&second, "before\n").unwrap();
    let services = Services::create(
        &BobConfig::default(),
        &directory,
        Arc::new(InMemoryMemories::default()),
    )
    .await
    .unwrap();
    let request = invocation(
        &services,
        "rename_symbol",
        json!({"filePath":"a.rs","line":1,"character":1,"newName":"new"}),
    );
    let plan = crate::tools::PreparedWrites::prepare(
        &directory,
        vec![
            (path.clone(), "old\n".into(), "new\n".into()),
            (second.clone(), "before\n".into(), "after\n".into()),
        ],
    )
    .unwrap();
    let action = ServiceAction::Lsp(
        json!({"kind":"writes", "name":"rename_symbol", "input":request.input, "plan":plan, "paths":[path, second]}),
    );
    let bound = BoundService {
        version: 1,
        invocation_sha256: invocation_digest(&request).unwrap(),
        action_sha256: action_digest(&request, &action).unwrap(),
        action,
    };
    let original = PreparedTool {
        invocation: request,
        specification: serde_json::to_value(bound).unwrap(),
        preview: String::new(),
        presentation: None,
    };
    services.prepared_capabilities(&original).unwrap();
    let requirements = services
        .prepared_authorization_requirements(&original)
        .unwrap();
    let targets = requirements
        .into_iter()
        .map(|requirement| match requirement {
            bob_kernel::PreparedAuthorizationRequirement::EditFile { path } => path,
            bob_kernel::PreparedAuthorizationRequirement::ToolPath { .. } => {
                panic!("LSP mutations must retain explicit per-file edit authorization")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(targets, vec![path.clone(), second.clone()]);
    let mut changed_invocation = original.clone();
    changed_invocation.invocation.operation_id = OperationId("other-operation".into());
    assert!(services
        .prepared_authorization_requirements(&changed_invocation)
        .is_err());
    let (_sender, cancel) = watch::channel(false);
    for pointer in [
        "/action/request/plan/files/0/after",
        "/action/request/plan/files/0/snapshot/sha256",
        "/action/request/plan/files/0/staged",
        "/action/request/plan/root",
        "/action/request/plan/root_identity",
        "/action/request/paths/0",
    ] {
        let mut tampered = original.clone();
        *tampered.specification.pointer_mut(pointer).expect(pointer) = json!("substituted");
        assert!(
            services.prepared_capabilities(&tampered).is_err(),
            "{pointer}"
        );
        assert!(
            services
                .prepared_authorization_requirements(&tampered)
                .is_err(),
            "{pointer}"
        );
        assert!(
            services.execute(tampered, cancel.clone()).await.is_err(),
            "{pointer}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "before\n");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
    }
    assert!(!services.execute(original, cancel).await.unwrap().is_error);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "after\n");
    drop(services);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn retained_text_is_not_destroyed_by_preview_budget_and_cancel_wait_is_bounded() {
    let text = "界".repeat(5000);
    let result = outcome(text.clone());
    assert_eq!(result.text, text);
    assert!(result.presentation.unwrap().body_preview.unwrap().len() < 4200);
    let (sender, receiver) = watch::channel(false);
    let mut waiting = Box::pin(cancelled(receiver));
    assert!(futures::poll!(&mut waiting).is_pending());
    sender.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap();
}
