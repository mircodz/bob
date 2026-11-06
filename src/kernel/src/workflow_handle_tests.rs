use super::*;
use crate::{
    Executor, Runtime, WorkflowPageRequest, WorkflowQuery, WorkflowQueryResult, WorkflowTextSource,
};
use async_trait::async_trait;
use bob_types::Completion;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::watch;

struct NoGeneration {
    calls: AtomicUsize,
    models: Vec<ModelInfo>,
}

#[async_trait]
impl Executor for NoGeneration {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        _: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("inspection must not generate")
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        unreachable!()
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        unreachable!()
    }
}

#[tokio::test]
async fn workflow_membership_and_handle_reads_survive_reopen_without_scheduling() {
    let (dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"title":"inspect λ", "max_concurrency":1,
        "steps":[{"id":"inspect","agent":{"prompt":"Read only the assigned fixture."}}]});
    let (run, workflow, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &workflow, &models, &input).is_none());
    let workflow_child = children(&store, &workflow)[0].clone();
    let other_operation = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'spawn_agent','tool','independent','{}','ready')", params![other_operation,run,root.thread]).unwrap();
    let profile = profile(&store, &root);
    let tx = store.db.transaction().unwrap();
    let collaborator = delegation::launch(
        &tx,
        &other_operation,
        &run,
        &root.thread,
        &profile,
        &models,
        Child {
            name: "workflow-999",
            prompt: "Independent work",
            role: "collaborator",
            subagent_type: None,
            model: None,
            read_only: true,
            blocking: false,
            schema: None,
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let details = crate::sessions::thread_details(&store.db, &root.session).unwrap();
    assert_eq!(
        details
            .iter()
            .find(|row| row.thread == workflow_child)
            .unwrap()
            .workflow_operation
            .as_ref(),
        Some(&workflow)
    );
    for thread in [&root.thread, &collaborator] {
        assert!(details
            .iter()
            .find(|row| &row.thread == thread)
            .unwrap()
            .workflow_operation
            .is_none());
    }
    drop(store);
    let executor = Arc::new(NoGeneration {
        calls: AtomicUsize::new(0),
        models,
    });
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    {
        let WorkflowQueryResult::List(page) = handle
            .workflow_query(
                root.session.clone(),
                WorkflowQuery::List {
                    page: WorkflowPageRequest::default(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected workflow listing")
        };
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].operation, workflow);
        assert_eq!(page.rows[0].owner, root.thread);
        let WorkflowQueryResult::Overview(overview) = handle
            .workflow_query(
                root.session.clone(),
                WorkflowQuery::Overview {
                    operation: workflow.clone(),
                    container: None,
                    page: WorkflowPageRequest::default(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected workflow overview")
        };
        assert_eq!(overview.nodes.rows[0].path, "root/inspect");
        assert_eq!(overview.nodes.rows[0].child.as_ref(), Some(&workflow_child));
        let WorkflowQueryResult::Text(text) = handle
            .workflow_query(
                root.session.clone(),
                WorkflowQuery::Text {
                    operation: workflow.clone(),
                    source: WorkflowTextSource::Definition,
                    offset: 0,
                    max_bytes: 4096,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected workflow definition")
        };
        assert_eq!(serde_json::from_str::<Value>(&text.text).unwrap(), input);
        assert!(text.next_offset.is_none());
        assert!(handle
            .workflow_query(
                SessionId::new(),
                WorkflowQuery::Node {
                    operation: workflow.clone(),
                    path: "root/inspect".into(),
                }
            )
            .await
            .is_err());
        let details = handle.thread_details(root.session.clone()).await.unwrap();
        assert_eq!(
            details
                .iter()
                .find(|row| row.thread == workflow_child)
                .unwrap()
                .workflow_operation
                .as_ref(),
            Some(&workflow)
        );
        assert!(details
            .iter()
            .find(|row| row.thread == collaborator)
            .unwrap()
            .workflow_operation
            .is_none());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }
    handle.shutdown().await.unwrap();
}

#[test]
#[ignore = "release-mode workflow query measurement; no provider calls"]
fn large_workflow_inspection_measurement() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let items: Vec<_> = (0..256).map(|index| format!("item-{index}")).collect();
    let input = json!({"title":"large workflow", "max_concurrency":4,
        "steps":[{"id":"batch", "fan_out":{"over":items, "prompt":"Inspect {item}", "repeat":4}}]});
    let (run, workflow, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &workflow, &models, &input).is_none());
    assert_eq!(children(&store, &workflow).len(), 4);
    let materialized: usize = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM workflow_nodes WHERE operation=?1",
            [&workflow],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(materialized, 1025);
    for indexed in [true, false] {
        if !indexed {
            store
                .db
                .execute_batch("DROP INDEX thread_workflows; DROP INDEX workflow_node_children;")
                .unwrap();
        }
        let mut samples = Vec::new();
        for _ in 0..100 {
            let start = std::time::Instant::now();
            let result = crate::workflow_view::query(
                &store.db,
                &root.session,
                &WorkflowQuery::Overview {
                    operation: workflow.clone(),
                    container: Some("root/batch".into()),
                    page: WorkflowPageRequest::default(),
                },
            )
            .unwrap();
            samples.push(start.elapsed());
            let WorkflowQueryResult::Overview(overview) = result else {
                panic!("expected overview")
            };
            assert!(overview.nodes.rows.len() <= 50);
            assert!(overview.nodes.next_cursor.is_some());
            assert!(serde_json::to_vec(&overview).unwrap().len() <= crate::WORKFLOW_MAX_PAGE_BYTES);
        }
        samples.sort_unstable();
        eprintln!(
            "1024-leaf workflow overview indexed={indexed}: p50={:?}, p95={:?}, p99={:?}",
            samples[50], samples[95], samples[99]
        );
    }
}

#[test]
#[ignore = "release-mode retained iteration measurement; no provider calls"]
fn retained_workflow_iteration_measurement() {
    let (_dir, mut store, root, _) = store::tests::fixture();
    let input = json!({"steps":[{"id":"rounds","loop":{"max":20,"steps":[{"id":"read","agent":{"prompt":"Read"}}]}}]});
    let (_, workflow, _) = operation(&store, &root, "workflow", input.clone());
    let tx = store.db.transaction().unwrap();
    let job = JobId::new();
    tx.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,?2,?3,id,run,'workflow','completed','Retained iterations',0,0 FROM operations WHERE id=?4", params![job,root.session,root.thread,workflow]).unwrap();
    tx.execute(
        "INSERT INTO workflows(operation,job,state) VALUES(?1,?2,'completed')",
        params![workflow, job],
    )
    .unwrap();
    for iteration in 0..20 {
        let output = json!({"output":{"A":"x".repeat(3*1024*1024),"_iterations":iteration+1,"_stop_reason":"max_rounds"},"error":null});
        let result = artifact(&tx, &encode(&output).unwrap()).unwrap();
        tx.execute("INSERT INTO workflow_iterations(operation,node,iteration,result) VALUES(?1,'root/rounds',?2,?3)", params![workflow,iteration,result]).unwrap();
        if iteration == 19 {
            tx.execute("INSERT INTO workflow_nodes(operation,path,state,specification,iteration,result) VALUES(?1,'root/rounds','completed',?2,19,?3)", params![workflow,encode(&input["steps"][0]).unwrap(),result]).unwrap();
        }
    }
    tx.commit().unwrap();
    let mut cursor = None;
    for page_index in 0..4 {
        let query = WorkflowQuery::Iterations {
            operation: workflow.clone(),
            path: "root/rounds".into(),
            page: WorkflowPageRequest {
                cursor: cursor.clone(),
                limit: 5,
                max_bytes: 8192,
            },
        };
        let mut samples = Vec::new();
        for _ in 0..30 {
            let start = std::time::Instant::now();
            let result = crate::workflow_view::query(&store.db, &root.session, &query).unwrap();
            samples.push(start.elapsed());
            let WorkflowQueryResult::Iterations(page) = result else {
                panic!("expected iterations")
            };
            assert_eq!(page.rows.len(), 5);
            for row in &page.rows {
                assert_eq!(
                    row.termination,
                    (row.iteration == 19).then_some(crate::WorkflowTermination::MaxRounds)
                );
            }
            cursor = page.next_cursor;
        }
        samples.sort_unstable();
        eprintln!(
            "20 x 3MiB retained iterations page={page_index}: p50={:?}, p95={:?}, p99={:?}",
            samples[15], samples[28], samples[29]
        );
    }
    assert!(cursor.is_none());
}
