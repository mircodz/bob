use super::*;

fn configure_read(actor: &mut Actor, root: &Receipt, decision: RuleDecision) {
    actor
        .accept_command(&Command::SetPermissions {
            id: CommandId::new(),
            session: root.session.clone(),
            expected_revision: 0,
            permissions: PermissionPolicy {
                rules: vec![PermissionRule {
                    tool: "read_file".into(),
                    decision,
                }],
                ..Default::default()
            },
        })
        .unwrap();
}

fn retain_preparation(actor: &mut Actor, tool: &PreparedTool) {
    insert_preparing(actor, tool);
    actor
        .finish(Finished {
            id: tool.invocation.operation_id.clone(),
            result: Ok(Output::Prepared(Box::new(tool.clone()))),
        })
        .unwrap();
}

#[test]
fn primary_target_grants_preserve_raw_input_and_read_effect_class() {
    let (_dir, store, root, models) = fixture();
    let executor = Arc::new(CheckpointExecutor::new(models));
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    configure_read(&mut actor, &root, RuleDecision::Ask);
    let mut tool = prepared(&actor, &root, "read_file");
    tool.invocation.input = json!({"path":"alternate-spelling.txt"});
    let target = Path::new(&tool.invocation.workspace).join("resolved.txt");
    *executor.target.lock().unwrap() = Some(target.clone());
    retain_preparation(&mut actor, &tool);
    let options =
        crate::approvals::options(&actor.store.db, &tool.invocation.operation_id, 1).unwrap();
    let grant = options
        .options
        .iter()
        .filter_map(|option| option.grant.clone())
        .find(|grant| grant.path_prefix.as_deref() == target.to_str())
        .expect("prepared target must be offered as a scoped grant");
    let (write, raw, retained): (bool, String, String) = actor
        .store
        .db
        .query_row(
            "SELECT effect_write,input,authorizations FROM operations WHERE id=?1",
            [&tool.invocation.operation_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(
        !write,
        "a primary path must not turn a read into an edit barrier"
    );
    assert_eq!(decode::<Value>(raw).unwrap(), tool.invocation.input);
    let requirements: Vec<PreparedAuthorizationRequirement> = decode(retained).unwrap();
    assert!(
        matches!(requirements.as_slice(), [PreparedAuthorizationRequirement::ToolPath { path }] if path == &target)
    );
    let widened = PermissionGrant {
        path_prefix: Some(tool.invocation.workspace.clone()),
        ..grant.clone()
    };
    assert!(actor
        .accept_command(&Command::DecideWithGrant {
            id: CommandId::new(),
            operation: tool.invocation.operation_id.clone(),
            revision: 1,
            approve: true,
            grant: Some(widened),
        })
        .is_err());
    actor
        .accept_command(&Command::DecideWithGrant {
            id: CommandId::new(),
            operation: tool.invocation.operation_id.clone(),
            revision: 1,
            approve: true,
            grant: Some(grant.clone()),
        })
        .unwrap();
    let permissions = crate::policy::permissions(&actor.store.db, &root.thread).unwrap();
    assert!(permissions.grants.contains(&grant));
    let snapshot = crate::sessions::session(&actor.store.db, &root.session).unwrap();
    let caps = executor.tool_capabilities("read_file");
    assert_eq!(
        actor
            .prepared_authorization(
                &snapshot.profile,
                &snapshot.policy,
                &permissions,
                &tool,
                &caps
            )
            .unwrap()
            .0,
        RuleDecision::Allow
    );
    assert_eq!(
        crate::policy::evaluate_in_workspace(
            &snapshot.policy,
            &permissions,
            &snapshot.profile,
            "read_file",
            &tool.invocation.input,
            &caps,
            &tool.invocation.workspace
        ),
        RuleDecision::Ask
    );
    assert_eq!(executor.executed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changed_primary_target_cannot_borrow_approval_or_allow_policy() {
    for allow in [false, true] {
        let (_dir, store, root, models) = fixture();
        let executor = Arc::new(CheckpointExecutor::new(models));
        let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
        configure_read(
            &mut actor,
            &root,
            if allow {
                RuleDecision::Allow
            } else {
                RuleDecision::Ask
            },
        );
        let tool = prepared(&actor, &root, "read_file");
        *executor.target.lock().unwrap() =
            Some(Path::new(&tool.invocation.workspace).join("first.txt"));
        retain_preparation(&mut actor, &tool);
        if !allow {
            actor
                .accept_command(&Command::Decide {
                    id: CommandId::new(),
                    operation: tool.invocation.operation_id.clone(),
                    revision: 1,
                    approve: true,
                })
                .unwrap();
        }
        *executor.target.lock().unwrap() =
            Some(Path::new(&tool.invocation.workspace).join("second.txt"));
        let (done, _) = mpsc::channel(64);
        let op = actor
            .next_operation(&tool.invocation.run_id)
            .unwrap()
            .unwrap();
        actor.dispatch_tool(op, &done).unwrap();
        assert!(actor.workers.is_empty());
        assert_eq!(executor.executed.load(Ordering::SeqCst), 0);
        assert_eq!(
            actor
                .store
                .db
                .query_row(
                    "SELECT state FROM operations WHERE id=?1",
                    [&tool.invocation.operation_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "failed"
        );
        let error: String = actor
            .store
            .db
            .query_row(
                "SELECT text FROM entries WHERE operation=?1 AND kind='tool_error'",
                [&tool.invocation.operation_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(error.contains("authorization targets changed"));
    }
}

#[test]
fn relative_primary_target_cannot_be_approved() {
    let (_dir, store, root, models) = fixture();
    let executor = Arc::new(CheckpointExecutor::new(models));
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    let tool = prepared(&actor, &root, "read_file");
    *executor.target.lock().unwrap() = Some("relative.txt".into());
    retain_preparation(&mut actor, &tool);
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                [&tool.invocation.operation_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "failed"
    );
    assert_eq!(
        actor
            .store
            .db
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE operation=?1",
                [&tool.invocation.operation_id],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        0
    );
    assert!(crate::policy::permissions(&actor.store.db, &root.thread)
        .unwrap()
        .grants
        .is_empty());
    assert_eq!(executor.executed.load(Ordering::SeqCst), 0);
}
