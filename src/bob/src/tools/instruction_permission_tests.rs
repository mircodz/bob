use super::*;

const CONVENTION: &str = "Scoped convention fixture: keep changes focused.";

fn instructions(path: Value) -> Step {
    Step::Reply(call("project_instructions", json!({"path": path})))
}

fn assert_retained(snapshot: &View) -> ArtifactId {
    assert!(snapshot.decisions.is_empty());
    let operation = snapshot
        .operations
        .iter()
        .find(|op| op.name == "project_instructions")
        .unwrap();
    assert_eq!(
        operation.state,
        OperationState::Done,
        "{:?}",
        snapshot.entries
    );
    let entries: Vec<_> = snapshot
        .entries
        .iter()
        .filter(|entry| entry.operation.as_ref() == Some(&operation.id))
        .collect();
    assert!(entries
        .iter()
        .any(|entry| entry.kind == EntryKind::ToolCall));
    assert!(entries
        .iter()
        .any(|entry| entry.kind == EntryKind::ToolResult && entry.text.contains(CONVENTION)));
    for entry in entries {
        let tool = entry.tool.as_ref().unwrap();
        assert_eq!(tool.state, OperationState::Done);
        assert_eq!(
            tool.presentation.as_ref().unwrap().summary.as_deref(),
            Some("1 convention sources")
        );
    }
    operation.result.clone().unwrap()
}

#[tokio::test]
async fn project_instructions_are_automatic_for_root_and_read_only_child_and_survive_reopen() {
    for plan in [false, true] {
        let dir = TestDir::new();
        fs::write(dir.workspace().join("AGENTS.md"), CONVENTION).unwrap();
        fs::create_dir(dir.workspace().join("nested")).unwrap();
        fs::create_dir(dir.0.join("home")).unwrap();
        let mut executor = ScriptedExecutor::new(
            vec![
                instructions(json!(".")),
                Step::Reply(call(
                    "launch_agent",
                    json!({
                        "name":"reader", "prompt":"Load scoped project conventions.",
                        "model":"worker", "read_only":true,
                    }),
                )),
                Step::WaitForChild,
                Step::Reply(text("parent finished")),
            ],
            vec![
                instructions(json!("nested")),
                Step::Reply(text("child finished")),
            ],
            None,
        );
        Arc::get_mut(&mut executor).unwrap().tools =
            ToolService::new(None, Some(dir.0.join("home")));
        let handle = open(&dir, executor.clone()).await;
        let root = session(
            &handle,
            &dir,
            "root",
            Policy {
                plan,
                ..Default::default()
            },
            false,
        )
        .await;
        submit(&handle, &root.thread).await;
        let settled = until(&handle, &root.thread, |snapshot| {
            assert!(
                snapshot.decisions.is_empty(),
                "routine convention read requested approval"
            );
            state(snapshot, &root.thread) == ThreadState::Completed
        })
        .await;
        let child = settled
            .threads
            .iter()
            .find(|thread| thread.parent_thread.as_ref() == Some(&root.thread))
            .unwrap();
        let child_id = child.id.clone();
        assert_eq!(child.state, ThreadState::Completed);
        let child_view = view(&handle, &child_id).await;
        let root_artifact = assert_retained(&settled);
        let child_artifact = assert_retained(&child_view);
        executor.assert_calls(2, 2);
        assert!(executor.tools.files.lock().await.reads.is_empty());
        {
            let observed = executor.observed.lock().unwrap();
            for model in ["root", "worker"] {
                let request = observed
                    .requests
                    .iter()
                    .filter(|r| r.model.id == model)
                    .nth(1)
                    .unwrap();
                assert!(request.messages.iter().flat_map(|message| &message.content).any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, is_error, .. }
                        if *is_error != Some(true) && content.as_text().is_some_and(|text| text.contains(CONVENTION)))
                }));
            }
        }
        bounded(handle.shutdown()).await.unwrap();
        let reopened = bounded(Runtime::open_paused(&dir.db(), executor.clone()))
            .await
            .unwrap();
        for (thread, artifact) in [(&root.thread, root_artifact), (&child_id, child_artifact)] {
            let restored = view(&reopened, thread).await;
            assert_eq!(assert_retained(&restored), artifact);
            assert!(artifact_text(&reopened, &artifact)
                .await
                .contains(CONVENTION));
        }
        bounded(reopened.shutdown()).await.unwrap();
        executor.assert_calls(2, 2);
        assert_eq!(
            fs::read_to_string(dir.workspace().join("AGENTS.md")).unwrap(),
            CONVENTION
        );
    }
}

#[tokio::test]
async fn external_project_instructions_still_require_approval_before_reading() {
    let dir = TestDir::new();
    let outside = dir.0.join("external");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("AGENTS.md"), CONVENTION).unwrap();
    let executor = ScriptedExecutor::new(
        vec![instructions(json!(outside)), Step::Reply(text("done"))],
        vec![],
        None,
    );
    let handle = open(&dir, executor.clone()).await;
    let root = session(&handle, &dir, "root", Policy::default(), false).await;
    submit(&handle, &root.thread).await;
    let pending = until(&handle, &root.thread, |snapshot| {
        !snapshot.decisions.is_empty()
    })
    .await;
    assert_eq!(pending.decisions.len(), 1);
    let decision = &pending.decisions[0];
    assert_eq!(decision.name, "project_instructions");
    assert_eq!(
        decision.presentation.as_ref().unwrap().target,
        Some(outside.display().to_string())
    );
    executor.assert_calls(1, 0);
    decide(&handle, decision, false).await;
    let settled = completed(&handle, &root.thread).await;
    assert_eq!(
        settled
            .operations
            .iter()
            .find(|op| op.id == decision.operation)
            .unwrap()
            .state,
        OperationState::Failed
    );
    executor.assert_calls(1, 0);
    assert!(executor.tools.files.lock().await.reads.is_empty());
    bounded(handle.shutdown()).await.unwrap();
}
