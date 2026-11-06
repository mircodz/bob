use crate::store::{artifact, tests::fixture, Store};
use crate::workflow_view::query;
use crate::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

fn total_changes(db: &Connection) -> i64 {
    db.query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap()
}

fn operation(db: &Connection, root: &Receipt, input: Value, state: &str) -> OperationId {
    let run = db
        .query_row(
            "SELECT id FROM runs WHERE thread=?1 AND state IN ('active','interrupted','stopping')",
            [&root.thread],
            |row| Ok(RunId(row.get(0)?)),
        )
        .optional()
        .unwrap()
        .unwrap_or_else(|| {
            let run = RunId::new();
            db.execute(
                "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                params![run, root.thread],
            )
            .unwrap();
            run
        });
    let operation = OperationId::new();
    db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'workflow','tool',?4,?5)",params![operation,run,root.thread,input.to_string(),state]).unwrap();
    operation
}
fn admit(db: &Connection, root: &Receipt, operation: &OperationId) -> JobId {
    let job = JobId::new();
    db.execute("INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) SELECT ?1,?2,?3,id,run,'workflow','running','Test workflow',0,0 FROM operations WHERE id=?4",params![job,root.session,root.thread,operation]).unwrap();
    db.execute(
        "INSERT INTO workflows(operation,job,state) VALUES(?1,?2,'running')",
        params![operation, job],
    )
    .unwrap();
    job
}
fn node(
    db: &Connection,
    operation: &OperationId,
    path: &str,
    state: &str,
    output: Option<Value>,
) -> Option<ArtifactId> {
    let result = output
        .map(|output| artifact(db, &json!({"output":output,"error":null}).to_string()).unwrap());
    db.execute("INSERT INTO workflow_nodes(operation,path,state,specification,result) VALUES(?1,?2,?3,'{}',?4)",params![operation,path,state,result]).unwrap();
    result
}
fn child(
    db: &Connection,
    root: &Receipt,
    operation: &OperationId,
    path: &str,
    state: &str,
) -> ThreadId {
    let thread = ThreadId::new();
    db.execute("INSERT INTO threads(id,session,name,profile) SELECT ?1,session,'worker',profile FROM threads WHERE id=?2",params![thread,root.thread]).unwrap();
    db.execute(
        "INSERT INTO agents(thread,role,blocking,operation,state) VALUES(?1,'workflow',0,?2,?3)",
        params![thread, operation, state],
    )
    .unwrap();
    db.execute(
        "UPDATE workflow_nodes SET child=?3 WHERE operation=?1 AND path=?2",
        params![operation, path, thread],
    )
    .unwrap();
    thread
}
fn overview(
    db: &Connection,
    root: &Receipt,
    operation: &OperationId,
    container: Option<&str>,
    page: WorkflowPageRequest,
) -> WorkflowOverview {
    let request = WorkflowQuery::Overview {
        operation: operation.clone(),
        container: container.map(str::to_owned),
        page,
    };
    let request: WorkflowQuery =
        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
    let WorkflowQueryResult::Overview(value) = query(db, &root.session, &request).unwrap() else {
        panic!("wrong response")
    };
    value
}
fn details(
    db: &Connection,
    root: &Receipt,
    operation: &OperationId,
    path: &str,
) -> WorkflowNodeDetails {
    let WorkflowQueryResult::Node(value) = query(
        db,
        &root.session,
        &WorkflowQuery::Node {
            operation: operation.clone(),
            path: path.into(),
        },
    )
    .unwrap() else {
        panic!("wrong response")
    };
    value
}
fn text(
    db: &Connection,
    root: &Receipt,
    operation: &OperationId,
    source: WorkflowTextSource,
    max_bytes: usize,
) -> String {
    let mut offset = 0;
    let mut joined = String::new();
    let mut total = None;
    loop {
        let WorkflowQueryResult::Text(page) = query(
            db,
            &root.session,
            &WorkflowQuery::Text {
                operation: operation.clone(),
                source: source.clone(),
                offset,
                max_bytes,
            },
        )
        .unwrap() else {
            panic!("wrong response")
        };
        assert!(serde_json::to_vec(&page).unwrap().len() <= max_bytes);
        assert_eq!(*total.get_or_insert(page.total_bytes), page.total_bytes);
        assert_eq!(page.offset, offset);
        joined.push_str(&page.text);
        let Some(next) = page.next_offset else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(joined.len(), total.unwrap());
    joined
}

#[test]
fn modified_list_cursor_cannot_exceed_encoded_response_budget() {
    let (_dir, store, root, _) = fixture();
    for _ in 0..3 {
        operation(&store.db, &root, json!({"steps":[]}), "pending");
    }
    let page = WorkflowPageRequest {
        limit: 1,
        max_bytes: 4096,
        cursor: None,
    };
    let WorkflowQueryResult::List(first) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::List { page: page.clone() },
    )
    .unwrap() else {
        panic!("expected list")
    };
    let mut cursor: Value = serde_json::from_str(first.next_cursor.as_ref().unwrap()).unwrap();
    cursor["key"] = json!("\u{0001}".repeat(600));
    let modified = serde_json::to_string(&cursor).unwrap();
    assert!(modified.len() < 4096);
    assert!(query(
        &store.db,
        &root.session,
        &WorkflowQuery::List {
            page: WorkflowPageRequest {
                cursor: Some(modified),
                ..page
            }
        }
    )
    .is_err());
}

#[test]
fn unadmitted_approval_denial_and_yielded_running_are_listed_without_graphs() {
    let (_dir, store, root, _) = fixture();
    let pending = operation(&store.db, &root, json!({"steps":[]}), "pending");
    let approval = operation(&store.db, &root, json!({"steps":[]}), "approval");
    let denied = operation(&store.db, &root, json!({"steps":[]}), "failed");
    store.db.execute("INSERT INTO decisions(operation,revision,preview,state) VALUES(?1,0,'denied','denied')",[&denied]).unwrap();
    let yielded = operation(
        &store.db,
        &root,
        json!({"max_concurrency":3,"steps":[]}),
        "done",
    );
    admit(&store.db, &root, &yielded);
    let WorkflowQueryResult::List(page) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::List {
            page: WorkflowPageRequest::default(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(
        page.rows
            .iter()
            .map(|row| &row.operation)
            .collect::<Vec<_>>(),
        vec![&yielded, &denied, &approval, &pending]
    );
    assert_eq!(
        page.rows.iter().map(|row| row.state).collect::<Vec<_>>(),
        vec![
            WorkflowState::Running,
            WorkflowState::Denied,
            WorkflowState::Approval,
            WorkflowState::Unadmitted
        ]
    );
    assert_eq!(page.rows[0].max_concurrency, 3);
    assert!(!page.rows[1].admitted);
}

#[test]
fn listing_rows_bytes_cursor_scope_and_snapshot_are_bounded() {
    let (_dir, store, root, _) = fixture();
    for _ in 0..110 {
        operation(
            &store.db,
            &root,
            json!({"title":"界".repeat(500),"steps":[]}),
            "pending",
        );
    }
    let page_request = WorkflowPageRequest {
        limit: 100,
        max_bytes: 4096,
        cursor: None,
    };
    let WorkflowQueryResult::List(first) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::List {
            page: page_request.clone(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert!(!first.rows.is_empty() && first.rows.len() < 100);
    assert!(serde_json::to_vec(&first).unwrap().len() <= 4096);
    let later = operation(&store.db, &root, json!({"steps":[]}), "pending");
    let mut seen: Vec<_> = first.rows.iter().map(|row| row.operation.clone()).collect();
    let mut cursor = first.next_cursor.clone();
    while cursor.is_some() {
        let WorkflowQueryResult::List(page) = query(
            &store.db,
            &root.session,
            &WorkflowQuery::List {
                page: WorkflowPageRequest {
                    cursor: cursor.clone(),
                    ..page_request.clone()
                },
            },
        )
        .unwrap() else {
            panic!()
        };
        assert!(serde_json::to_vec(&page).unwrap().len() <= 4096);
        seen.extend(page.rows.into_iter().map(|row| row.operation));
        assert_ne!(cursor, page.next_cursor);
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 110);
    assert!(!seen.contains(&later));
    assert!(query(
        &store.db,
        &root.session,
        &WorkflowQuery::Agents {
            operation: later.clone(),
            page: WorkflowPageRequest {
                cursor: first.next_cursor,
                ..page_request
            }
        }
    )
    .is_err());
    for limit in [0, 101] {
        assert!(query(
            &store.db,
            &root.session,
            &WorkflowQuery::List {
                page: WorkflowPageRequest {
                    limit,
                    ..Default::default()
                }
            }
        )
        .is_err());
    }
}

#[test]
fn declared_order_future_dependencies_capacity_and_failure_are_distinct() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"z","agent":{"prompt":"one"}},{"id":"a","agent":{"prompt":"two"}},{"id":"b","agent":{"prompt":"three"}}]}),
        "waiting",
    );
    admit(&store.db, &root, &op);
    node(&store.db, &op, "root/z", "waiting_capacity", None);
    let first = overview(
        &store.db,
        &root,
        &op,
        None,
        WorkflowPageRequest {
            limit: 1,
            ..Default::default()
        },
    );
    assert_eq!(first.nodes.rows[0].path, "root/z");
    assert_eq!(first.nodes.rows[0].state, WorkflowState::WaitingCapacity);
    let next = overview(
        &store.db,
        &root,
        &op,
        None,
        WorkflowPageRequest {
            cursor: first.nodes.next_cursor,
            ..Default::default()
        },
    );
    assert_eq!(next.nodes.rows[0].path, "root/a");
    assert_eq!(next.nodes.rows[0].state, WorkflowState::Future);
    assert!(!next.nodes.rows[0].instantiated);
    let dependent = details(&store.db, &root, &op, "root/a");
    assert_eq!(dependent.dependencies[0].path, "root/z");
    assert!(!dependent.blockers.is_empty());
    store
        .db
        .execute(
            "UPDATE workflow_nodes SET state='failed' WHERE operation=?1",
            [&op],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE workflows SET state='failed' WHERE operation=?1",
            [&op],
        )
        .unwrap();
    assert_eq!(
        details(&store.db, &root, &op, "root/a").node.state,
        WorkflowState::Blocked
    );
    assert_eq!(
        details(&store.db, &root, &op, "root/b").node.state,
        WorkflowState::Skipped
    );
}

#[test]
fn conditional_choice_hides_unselected_admissions_and_completes_empty_else() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"choose","conditional":{"if":"$check.ok","then":[{"id":"forbidden","agent":{"prompt":"never"}}]}}]}),
        "done",
    );
    admit(&store.db, &root, &op);
    node(
        &store.db,
        &op,
        "root/choose",
        "completed",
        Some(json!({"branch":"else","output":{}})),
    );
    node(
        &store.db,
        &op,
        "root/choose/!choice",
        "completed",
        Some(json!({"condition":false,"branch":"else"})),
    );
    let branches = overview(
        &store.db,
        &root,
        &op,
        Some("root/choose"),
        Default::default(),
    );
    assert_eq!(branches.nodes.rows.len(), 2);
    assert_eq!(branches.nodes.rows[0].state, WorkflowState::NotSelected);
    assert_eq!(branches.nodes.rows[1].state, WorkflowState::Completed);
    let forbidden = details(&store.db, &root, &op, "root/choose/then/forbidden");
    assert_eq!(forbidden.node.state, WorkflowState::NotSelected);
    assert!(!forbidden.node.instantiated);
    assert!(forbidden.node.child.is_none());
    assert_eq!(
        details(&store.db, &root, &op, "root/choose")
            .node
            .selected_branch
            .as_deref(),
        Some("else")
    );
    let rows = overview(
        &store.db,
        &root,
        &op,
        Some("root/choose/else"),
        Default::default(),
    )
    .nodes;
    assert!(rows.rows.is_empty() && rows.next_cursor.is_none());
}

#[test]
fn container_children_membership_and_stopping_occupancy_are_authoritative() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"max_concurrency":2,"steps":[{"id":"fan","fan_out":{"over":["a","b"],"prompt":"{item}"}}]}),
        "done",
    );
    admit(&store.db, &root, &op);
    node(&store.db, &op, "root/fan", "pending", None);
    node(&store.db, &op, "root/fan/0/0", "running", None);
    node(&store.db, &op, "root/fan/1/0", "waiting_capacity", None);
    let worker = child(&store.db, &root, &op, "root/fan/0/0", "completed");
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'stopping')",
            params![RunId::new(), worker],
        )
        .unwrap();
    let other = operation(&store.db, &root, json!({"steps":[]}), "done");
    store
        .db
        .execute(
            "UPDATE agents SET operation=?2 WHERE thread=?1",
            params![worker, other],
        )
        .unwrap();
    let overview = overview(&store.db, &root, &op, None, Default::default());
    assert_eq!(overview.workflow.active_agents, 1);
    assert_eq!(overview.nodes.rows[0].state, WorkflowState::Stopping);
    assert_eq!(overview.workflow.state, WorkflowState::Running);
    let WorkflowQueryResult::Agents(agents) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::Agents {
            operation: op.clone(),
            page: Default::default(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(agents.rows.len(), 1);
    assert_eq!(agents.rows[0].thread, worker);
    assert_eq!(agents.rows[0].node.as_deref(), Some("root/fan/0/0"));
    let group = details(&store.db, &root, &op, "root/fan/0");
    assert!(group.node.expandable);
    assert!(text(
        &store.db,
        &root,
        &op,
        WorkflowTextSource::NodeSpecification {
            path: "root/fan/0".into()
        },
        1024
    )
    .contains("execution_group"));
}

#[test]
fn cancellation_unknown_and_completed_nodes_keep_separate_meanings() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"done","agent":{"prompt":"done"}},{"id":"next","agent":{"prompt":"later"}}]}),
        "done",
    );
    let job = admit(&store.db, &root, &op);
    node(
        &store.db,
        &op,
        "root/done",
        "completed",
        Some(json!("kept")),
    );
    for (raw, state) in [
        ("stopping", WorkflowState::Stopping),
        ("cancelled", WorkflowState::Cancelled),
        ("unknown", WorkflowState::Unknown),
    ] {
        store
            .db
            .execute("UPDATE jobs SET state=?2 WHERE id=?1", params![job, raw])
            .unwrap();
        let view = overview(&store.db, &root, &op, None, Default::default());
        assert_eq!(view.workflow.state, state);
        assert_eq!(view.nodes.rows[0].state, WorkflowState::Completed);
        assert_eq!(view.nodes.rows[1].state, state);
    }
}

#[test]
fn loop_pages_only_materialized_passes_and_distinguish_goal_from_cap() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"repeat","loop":{"max":20,"until":"$check.ok","steps":[{"id":"check","agent":{"prompt":"check"}}]}}]}),
        "done",
    );
    admit(&store.db, &root, &op);
    let cap = node(
        &store.db,
        &op,
        "root/repeat",
        "completed",
        Some(json!({"_iterations":2,"_stop_reason":"max_rounds"})),
    )
    .unwrap();
    node(
        &store.db,
        &op,
        "root/repeat/pass0/check",
        "completed",
        Some(json!({"ok":false})),
    );
    node(
        &store.db,
        &op,
        "root/repeat/pass1/check",
        "completed",
        Some(json!({"ok":false})),
    );
    store
        .db
        .execute(
            "UPDATE workflow_nodes SET iteration=1 WHERE operation=?1 AND path='root/repeat'",
            [&op],
        )
        .unwrap();
    for iteration in 0..2 {
        store.db.execute("INSERT INTO workflow_iterations(operation,node,iteration,result) VALUES(?1,'root/repeat',?2,?3)",params![op,iteration,cap]).unwrap();
    }
    let view = overview(
        &store.db,
        &root,
        &op,
        Some("root/repeat"),
        Default::default(),
    );
    assert_eq!(
        view.nodes
            .rows
            .iter()
            .map(|node| node.path.as_str())
            .collect::<Vec<_>>(),
        vec!["root/repeat/pass0", "root/repeat/pass1"]
    );
    assert_eq!(
        view.nodes
            .rows
            .iter()
            .map(|node| node.iteration)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        details(&store.db, &root, &op, "root/repeat/pass1")
            .node
            .iteration,
        1
    );
    let detail = details(&store.db, &root, &op, "root/repeat");
    assert_eq!(
        detail.node.termination,
        Some(WorkflowTermination::MaxRounds)
    );
    assert_eq!(detail.node.iteration, 1);
    let WorkflowQueryResult::Iterations(page) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::Iterations {
            operation: op.clone(),
            path: "root/repeat".into(),
            page: WorkflowPageRequest {
                limit: 1,
                ..Default::default()
            },
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(page.rows[0].iteration, 0);
    assert!(page.next_cursor.is_some());
    let met = artifact(&store.db,&json!({"output":{"_stop_reason":"condition_met","payload":"x".repeat(100_000)},"error":null}).to_string()).unwrap();
    store
        .db
        .execute(
            "UPDATE workflow_nodes SET result=?2 WHERE operation=?1 AND path='root/repeat'",
            params![op, met],
        )
        .unwrap();
    assert_eq!(
        details(&store.db, &root, &op, "root/repeat")
            .node
            .termination,
        Some(WorkflowTermination::ConditionMet)
    );
}

#[test]
fn session_isolation_and_artifact_allowlist_exclude_private_records() {
    let (_dir, store, root, _) = fixture();
    let op = operation(&store.db, &root, json!({"steps":[]}), "done");
    admit(&store.db, &root, &op);
    let public = node(
        &store.db,
        &op,
        "root/one",
        "completed",
        Some(json!("public")),
    )
    .unwrap();
    let private = artifact(&store.db, "private provider request and hidden reasoning").unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET prepared=?2 WHERE id=?1",
            params![op, private.0],
        )
        .unwrap();
    let foreign = SessionId::new();
    store
        .db
        .execute(
            "INSERT INTO sessions(id,workspace,policy) VALUES(?1,'.','{}')",
            [&foreign],
        )
        .unwrap();
    for session in [&root.session, &foreign] {
        assert!(query(
            &store.db,
            session,
            &WorkflowQuery::Text {
                operation: op.clone(),
                source: WorkflowTextSource::Artifact {
                    artifact: private.clone()
                },
                offset: 0,
                max_bytes: 1024
            }
        )
        .is_err());
    }
    assert!(query(
        &store.db,
        &foreign,
        &WorkflowQuery::Text {
            operation: op.clone(),
            source: WorkflowTextSource::Artifact {
                artifact: public.clone()
            },
            offset: 0,
            max_bytes: 1024
        }
    )
    .is_err());
    assert!(text(
        &store.db,
        &root,
        &op,
        WorkflowTextSource::Artifact { artifact: public },
        1024
    )
    .contains("public"));
    let WorkflowQueryResult::List(list) = query(
        &store.db,
        &foreign,
        &WorkflowQuery::List {
            page: Default::default(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert!(list.rows.is_empty());
}

#[test]
fn unicode_text_crosses_chunk_boundaries_and_queries_survive_reopen_without_writes() {
    let (dir, store, root, _) = fixture();
    let original = format!("{}{}", "a".repeat(65535), "界🦀\n\u{0001}".repeat(400));
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"one","agent":{"prompt":original}}]}),
        "done",
    );
    admit(&store.db, &root, &op);
    let output = artifact(&store.db, &original).unwrap();
    store
        .db
        .execute(
            "UPDATE workflows SET result=?2,state='completed' WHERE operation=?1",
            params![op, output],
        )
        .unwrap();
    let changes = total_changes(&store.db);
    assert_eq!(
        text(
            &store.db,
            &root,
            &op,
            WorkflowTextSource::Artifact {
                artifact: output.clone()
            },
            4096
        ),
        original
    );
    let definition = text(&store.db, &root, &op, WorkflowTextSource::Definition, 4096);
    assert!(definition.len() > 65536);
    assert!(overview(&store.db, &root, &op, None, Default::default()).definition_available);
    let spec_before = text(
        &store.db,
        &root,
        &op,
        WorkflowTextSource::NodeSpecification {
            path: "root/one".into(),
        },
        4096,
    );
    assert_eq!(total_changes(&store.db), changes);
    assert!(query(
        &store.db,
        &root.session,
        &WorkflowQuery::Text {
            operation: op.clone(),
            source: WorkflowTextSource::Artifact {
                artifact: output.clone()
            },
            offset: 65536,
            max_bytes: 1024
        }
    )
    .is_err());
    drop(store);
    let reopened = Store::open(&dir.0.join("lab.db")).unwrap();
    let changes = total_changes(&reopened.db);
    assert_eq!(
        text(
            &reopened.db,
            &root,
            &op,
            WorkflowTextSource::Artifact { artifact: output },
            4096
        ),
        original
    );
    assert_eq!(
        text(
            &reopened.db,
            &root,
            &op,
            WorkflowTextSource::NodeSpecification {
                path: "root/one".into()
            },
            4096
        ),
        spec_before
    );
    assert_eq!(total_changes(&reopened.db), changes);
}

#[test]
fn oversize_definition_does_not_break_listing_and_retains_paged_source() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"title":"large","steps":[{"id":"one","agent":{"prompt":"x".repeat(WORKFLOW_MAX_DEFINITION_BYTES)}}]}),
        "failed",
    );
    let view = overview(&store.db, &root, &op, None, Default::default());
    assert!(!view.definition_available);
    assert!(view.definition_error.unwrap().contains("4 MiB"));
    let WorkflowQueryResult::List(list) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::List {
            page: Default::default(),
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(list.rows[0].title, "large");
    let WorkflowQueryResult::Text(page) = query(
        &store.db,
        &root.session,
        &WorkflowQuery::Text {
            operation: op,
            source: WorkflowTextSource::Definition,
            offset: 0,
            max_bytes: 1024,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert!(page.total_bytes > WORKFLOW_MAX_DEFINITION_BYTES);
    assert!(page.next_offset.is_some());
}

#[test]
fn dynamic_pages_keep_original_membership_when_more_nodes_are_admitted() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"shape":"fan_out","items":["a","b","c"],"map_prompt":"{item}"}),
        "done",
    );
    admit(&store.db, &root, &op);
    node(&store.db, &op, "map/0", "completed", Some(json!("a")));
    node(&store.db, &op, "map/1", "running", None);
    let first = overview(
        &store.db,
        &root,
        &op,
        Some("map"),
        WorkflowPageRequest {
            limit: 1,
            ..Default::default()
        },
    );
    assert_eq!(first.nodes.rows[0].path, "map/0");
    node(&store.db, &op, "map/2", "waiting_capacity", None);
    let second = overview(
        &store.db,
        &root,
        &op,
        Some("map"),
        WorkflowPageRequest {
            cursor: first.nodes.next_cursor,
            ..Default::default()
        },
    );
    assert_eq!(second.nodes.rows.len(), 1);
    assert_eq!(second.nodes.rows[0].path, "map/1");
    assert!(second.nodes.next_cursor.is_none());
}

#[test]
fn settled_workflow_is_not_reactivated_by_child_followup_cancellation() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"shape":"fan_out","items":["a"],"map_prompt":"{item}"}),
        "done",
    );
    admit(&store.db, &root, &op);
    let retained = node(
        &store.db,
        &op,
        "map/0",
        "completed",
        Some(json!("original report")),
    )
    .unwrap();
    let worker = child(&store.db, &root, &op, "map/0", "completed");
    store
        .db
        .execute(
            "UPDATE agents SET outcome=?2 WHERE thread=?1",
            params![worker, retained],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE workflows SET state='completed',result=?2 WHERE operation=?1",
            params![op, retained],
        )
        .unwrap();
    let before = overview(&store.db, &root, &op, None, Default::default());
    let before_node = details(&store.db, &root, &op, "map/0");
    let before_agents = query(
        &store.db,
        &root.session,
        &WorkflowQuery::Agents {
            operation: op.clone(),
            page: Default::default(),
        },
    )
    .unwrap();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'stopping')",
            params![RunId::new(), worker],
        )
        .unwrap();
    let after = overview(&store.db, &root, &op, None, Default::default());
    assert_eq!(after.workflow.state, WorkflowState::Completed);
    assert_eq!(after.workflow.active_agents, 0);
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    assert_eq!(
        serde_json::to_value(before_node).unwrap(),
        serde_json::to_value(details(&store.db, &root, &op, "map/0")).unwrap()
    );
    assert_eq!(
        serde_json::to_value(before_agents).unwrap(),
        serde_json::to_value(
            query(
                &store.db,
                &root.session,
                &WorkflowQuery::Agents {
                    operation: op.clone(),
                    page: Default::default()
                }
            )
            .unwrap()
        )
        .unwrap()
    );
    assert!(text(
        &store.db,
        &root,
        &op,
        WorkflowTextSource::Artifact { artifact: retained },
        1024
    )
    .contains("original report"));
}

#[test]
fn permission_human_and_recovery_blockers_link_the_child() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"one","agent":{"prompt":"one"}}]}),
        "waiting",
    );
    admit(&store.db, &root, &op);
    node(&store.db, &op, "root/one", "running", None);
    let worker = child(&store.db, &root, &op, "root/one", "running");
    let child_root = Receipt {
        thread: worker.clone(),
        ..root.clone()
    };
    let control = operation(&store.db, &child_root, json!({}), "approval");
    let permission = details(&store.db, &root, &op, "root/one");
    assert_eq!(permission.node.state, WorkflowState::WaitingPermission);
    assert_eq!(permission.blockers[0].thread, Some(worker.clone()));
    store
        .db
        .execute(
            "UPDATE operations SET state='waiting' WHERE id=?1",
            [&control],
        )
        .unwrap();
    store.db.execute("INSERT INTO human_controls(operation,kind,revision,epoch,payload,state) VALUES(?1,'question',0,0,'{}','pending')",[&control]).unwrap();
    assert_eq!(
        details(&store.db, &root, &op, "root/one").node.state,
        WorkflowState::WaitingHuman
    );
    store
        .db
        .execute(
            "UPDATE runs SET state='interrupted' WHERE thread=?1",
            [&worker],
        )
        .unwrap();
    assert_eq!(
        details(&store.db, &root, &op, "root/one").node.state,
        WorkflowState::Unknown
    );
    assert_eq!(
        overview(&store.db, &root, &op, None, Default::default())
            .workflow
            .state,
        WorkflowState::Unknown
    );
}

#[test]
fn all_public_result_links_are_allowed_but_provider_and_display_artifacts_are_not() {
    let (_dir, store, root, _) = fixture();
    let op = operation(&store.db, &root, json!({"steps":[]}), "done");
    admit(&store.db, &root, &op);
    node(&store.db, &op, "root/leaf", "completed", None);
    let worker = child(&store.db, &root, &op, "root/leaf", "completed");
    let aggregate = artifact(&store.db, "aggregate").unwrap();
    let child_report = artifact(&store.db, "child report").unwrap();
    let iteration = artifact(&store.db, "iteration").unwrap();
    let tool_output = artifact(&store.db, "tool output").unwrap();
    let private = artifact(&store.db, "private display or model output").unwrap();
    store
        .db
        .execute(
            "UPDATE workflows SET result=?2 WHERE operation=?1",
            params![op, aggregate],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE agents SET outcome=?2 WHERE thread=?1",
            params![worker, child_report],
        )
        .unwrap();
    store.db.execute("INSERT INTO workflow_iterations(operation,node,iteration,result) VALUES(?1,'root/loop',0,?2)",params![op,iteration]).unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET result=?2,display_result=?3 WHERE id=?1",
            params![op, tool_output, private],
        )
        .unwrap();
    for (id, expected) in [
        (aggregate, "aggregate"),
        (child_report, "child report"),
        (iteration, "iteration"),
        (tool_output, "tool output"),
    ] {
        assert_eq!(
            text(
                &store.db,
                &root,
                &op,
                WorkflowTextSource::Artifact { artifact: id },
                1024
            ),
            expected
        );
    }
    assert!(query(
        &store.db,
        &root.session,
        &WorkflowQuery::Text {
            operation: op,
            source: WorkflowTextSource::Artifact { artifact: private },
            offset: 0,
            max_bytes: 1024
        }
    )
    .is_err());
}

#[test]
fn oversized_row_fails_without_a_nonadvancing_cursor_and_spec_source_is_stable() {
    let (_dir, store, root, _) = fixture();
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"x".repeat(1000),"agent":{"prompt":"one"}}]}),
        "pending",
    );
    assert!(query(
        &store.db,
        &root.session,
        &WorkflowQuery::Overview {
            operation: op,
            container: None,
            page: WorkflowPageRequest {
                max_bytes: 4096,
                ..Default::default()
            }
        }
    )
    .is_err());
    let op = operation(
        &store.db,
        &root,
        json!({"steps":[{"id":"one","agent":{"prompt":"a".repeat(2048)}}]}),
        "waiting",
    );
    admit(&store.db, &root, &op);
    let before = text(
        &store.db,
        &root,
        &op,
        WorkflowTextSource::NodeSpecification {
            path: "root/one".into(),
        },
        1024,
    );
    node(&store.db, &op, "root/one", "pending", None);
    store
        .db
        .execute(
            "UPDATE workflow_nodes SET specification=?2 WHERE operation=?1",
            params![
                op,
                json!({"agent":{"prompt":"a".repeat(2048),"schema":null}}).to_string()
            ],
        )
        .unwrap();
    assert_eq!(
        text(
            &store.db,
            &root,
            &op,
            WorkflowTextSource::NodeSpecification {
                path: "root/one".into()
            },
            1024
        ),
        before
    );
}

#[test]
fn handle_query_contract_has_typed_send_future() {
    fn typed_future<T: std::future::Future<Output = anyhow::Result<WorkflowQueryResult>> + Send>(
        _: T,
    ) {
    }
    let _contract = |handle: &Handle, session: SessionId| {
        typed_future(handle.workflow_query(
            session,
            WorkflowQuery::List {
                page: Default::default(),
            },
        ));
    };
}
