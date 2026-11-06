use crate::delegation;
use crate::model::*;
use crate::store::{self, artifact, decode, encode, full_artifact, Store};
use rusqlite::params;
use serde_json::{json, Value};

fn operation(store: &Store, root: &Receipt, input: &Value) -> (RunId, OperationId) {
    let run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, root.thread],
        )
        .unwrap();
    let op = sibling_operation(store, root, &run, input);
    (run, op)
}

fn sibling_operation(store: &Store, root: &Receipt, run: &RunId, input: &Value) -> OperationId {
    let op = OperationId::new();
    store.db.execute(
        "INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'workflow','tool',?1,?4,'ready')",
        params![op, run, root.thread, encode(input).unwrap()],
    ).unwrap();
    op
}

fn tick(
    store: &mut Store,
    root: &Receipt,
    run: &RunId,
    op: &OperationId,
    models: &[ModelInfo],
    input: &Value,
) -> Option<Value> {
    let profile: Profile = decode(
        store
            .db
            .query_row(
                "SELECT profile FROM threads WHERE id=?1",
                [&root.thread],
                |r| r.get(0),
            )
            .unwrap(),
    )
    .unwrap();
    let tx = store.db.transaction().unwrap();
    let result = super::execute(&tx, op, run, &root.thread, &profile, models, input).unwrap();
    tx.commit().unwrap();
    result.map(|text| serde_json::from_str(&text).unwrap())
}

fn children(store: &Store, op: &OperationId) -> Vec<(String, ThreadId)> {
    store.db.prepare(
        "SELECT n.path,a.thread FROM agents a JOIN workflow_nodes n ON n.operation=a.operation AND n.child=a.thread WHERE a.operation=?1 ORDER BY a.rowid",
    ).unwrap().query_map([op], |r| Ok((r.get(0)?, ThreadId(r.get(1)?))))
        .unwrap().collect::<Result<_, _>>().unwrap()
}

fn pending(store: &Store, op: &OperationId) -> Vec<ThreadId> {
    store.db.prepare("SELECT thread FROM agents WHERE operation=?1 AND state IN ('queued','running') ORDER BY rowid")
        .unwrap().query_map([op], |r| Ok(ThreadId(r.get(0)?)))
        .unwrap().collect::<Result<_, _>>().unwrap()
}

fn done(store: &mut Store, child: &ThreadId, state: &str, text: &str) {
    let tx = store.db.transaction().unwrap();
    delegation::settle(&tx, child, state, text).unwrap();
    tx.execute(
        "UPDATE submissions SET state='settled' WHERE thread=?1",
        [child],
    )
    .unwrap();
    tx.commit().unwrap();
}

fn node_result(store: &Store, op: &OperationId, path: &str) -> (ArtifactId, Value) {
    let id = ArtifactId(
        store
            .db
            .query_row(
                "SELECT result FROM workflow_nodes WHERE operation=?1 AND path=?2",
                params![op, path],
                |r| r.get(0),
            )
            .unwrap(),
    );
    let value = decode(full_artifact(&store.db, &id).unwrap()).unwrap();
    (id, value)
}

fn artifacts(store: &Store) -> usize {
    store
        .db
        .query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))
        .unwrap()
}

fn conditional_input(otherwise: bool) -> Value {
    let mut conditional =
        json!({"if":"$check.ok","then":[{"id":"yes","agent":{"prompt":"Selected then"}}]});
    if otherwise {
        conditional["else"] = json!([{"id":"no","agent":{"prompt":"Selected else"}}]);
    }
    json!({"max_concurrency":1,"steps":[
        {"id":"check","agent":{"prompt":"Check","schema":{"type":"object"}}},
        {"id":"decision","conditional":conditional}
    ]})
}

#[test]
fn conditional_true_false_and_missing_else_have_explicit_outputs_and_cached_choices() {
    for (condition, otherwise) in [(true, true), (false, true), (false, false)] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let input = conditional_input(otherwise);
        let (run, op) = operation(&store, &root, &input);
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        let check = children(&store, &op)[0].1.clone();
        done(
            &mut store,
            &check,
            "completed",
            &json!({"ok":condition}).to_string(),
        );
        let first = tick(&mut store, &root, &run, &op, &models, &input);
        let branch = if condition { "then" } else { "else" };
        let choice = node_result(&store, &op, "root/decision/!choice");
        assert_eq!(
            choice.1,
            json!({"output":{"condition":condition,"branch":branch},"error":null})
        );
        let selected = children(&store, &op);
        let output = if condition || otherwise {
            assert!(first.is_none());
            assert_eq!(selected.len(), 2);
            let id = if condition { "yes" } else { "no" };
            assert_eq!(selected[1].0, format!("root/decision/{branch}/{id}"));
            let count = artifacts(&store);
            for _ in 0..5 {
                assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
                assert_eq!(children(&store, &op), selected);
                assert_eq!(artifacts(&store), count);
                assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
            }
            done(&mut store, &selected[1].1, "completed", "chosen output");
            let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
            assert_eq!(output["output"]["decision"]["output"][id], "chosen output");
            output
        } else {
            assert_eq!(selected.len(), 1);
            let output = first.unwrap();
            assert_eq!(output["output"]["decision"]["output"], json!({}));
            output
        };
        assert_eq!(output["status"], "success");
        assert_eq!(output["output"]["decision"]["branch"], branch);
        let count = artifacts(&store);
        assert_eq!(
            tick(&mut store, &root, &run, &op, &models, &input),
            Some(output)
        );
        assert_eq!(artifacts(&store), count);
    }
}

#[test]
fn conditional_rejects_nonboolean_and_missing_condition_without_child_admission() {
    for check_output in [
        json!({"ok":"true"}),
        json!({"ok":1}),
        json!({"ok":null}),
        json!({}),
    ] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let input = conditional_input(true);
        let (run, op) = operation(&store, &root, &input);
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        let check = children(&store, &op)[0].1.clone();
        done(&mut store, &check, "completed", &check_output.to_string());
        let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
        assert_eq!(output["status"], "failed");
        assert_eq!(children(&store, &op).len(), 1);
        let choices: usize = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM workflow_nodes WHERE operation=?1 AND path LIKE '%/!choice'",
                [&op],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(choices, 0);
    }
}

#[test]
fn conditional_only_preflights_selected_dynamic_inputs_even_inside_parallel() {
    for condition in [true, false] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let input = json!({"steps":[
            {"id":"check","agent":{"prompt":"Check","schema":{"type":"object"}}},
            {"id":"group","parallel":{"branches":{"choice":{"conditional":{
                "if":"$check.ok",
                "then":[{"id":"valid","fan_out":{"over":[],"prompt":"{item}"}}],
                "else":[{"id":"invalid","fan_out":{"over":"$check.not_array","prompt":"{item}"}}]
            }}}}}
        ]});
        let (run, op) = operation(&store, &root, &input);
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        let check = children(&store, &op)[0].1.clone();
        done(
            &mut store,
            &check,
            "completed",
            &json!({"ok":condition,"not_array":17}).to_string(),
        );
        let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
        assert_eq!(
            output["status"],
            if condition { "success" } else { "failed" }
        );
        assert_eq!(children(&store, &op).len(), 1);
        assert_eq!(
            node_result(&store, &op, "root/group/choice/!choice").1["output"]["condition"],
            condition
        );
        if condition {
            let unselected: usize = store.db.query_row(
                "SELECT COUNT(*) FROM workflow_nodes WHERE operation=?1 AND path LIKE 'root/group/choice/else/%'", [&op], |r| r.get(0),
            ).unwrap();
            assert_eq!(unselected, 0);
        }
    }
}

#[test]
fn committed_choice_is_not_reevaluated_while_children_wait() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = conditional_input(true);
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let check = children(&store, &op)[0].1.clone();
    done(&mut store, &check, "completed", r#"{"ok":true}"#);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let selected = children(&store, &op);
    let choice = node_result(&store, &op, "root/decision/!choice");
    let changed = artifact(&store.db, &json!({"output":{},"error":null}).to_string()).unwrap();
    store
        .db
        .execute(
            "UPDATE workflow_nodes SET result=?2 WHERE operation=?1 AND path='root/check'",
            params![op, changed],
        )
        .unwrap();
    for _ in 0..3 {
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        assert_eq!(children(&store, &op), selected);
        assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
    }
    done(&mut store, &selected[1].1, "completed", "kept");
    let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(
        output["output"]["decision"],
        json!({"branch":"then","output":{"yes":"kept"}})
    );
}

#[test]
fn nested_operations_share_one_budget_and_admit_each_leaf_once() {
    for limit in [1, 3] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let input = json!({"max_concurrency":limit,"steps":[
            {"id":"check","agent":{"prompt":"Check","schema":{"type":"object"}}},
            {"id":"group","parallel":{"branches":{
                "a_fan":{"fan_out":{"over":["a","b","c"],"prompt":"{item}"}},
                "b_loop":{"loop":{"max":2,"steps":[{"id":"batch","fan_out":{"over":["d","e"],"prompt":"{item}"}}]}},
                "c_choice":{"conditional":{"if":"$check.ok","then":[{"id":"batch","fan_out":{"over":["f","g"],"prompt":"{item}"}}]}}
            }}}
        ]});
        let (run, op) = operation(&store, &root, &input);
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        let check = children(&store, &op)[0].1.clone();
        done(&mut store, &check, "completed", r#"{"ok":true}"#);
        let mut final_output = None;
        for _ in 0..50 {
            if let Some(output) = tick(&mut store, &root, &run, &op, &models, &input) {
                final_output = Some(output);
                break;
            }
            let active = pending(&store, &op);
            assert!(active.len() <= limit);
            if active.len() == limit {
                let before = children(&store, &op);
                assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
                assert_eq!(children(&store, &op), before);
            }
            if let Some(child) = active.first() {
                done(&mut store, child, "completed", "work done");
            }
        }
        let output = final_output.expect("workflow must finish within bounded ticks");
        assert_eq!(output["status"], "success");
        assert_eq!(output["output"]["group"]["b_loop"]["_iterations"], 2);
        assert_eq!(
            output["output"]["group"]["b_loop"]["_stop_reason"],
            "max_rounds"
        );
        let all = children(&store, &op);
        assert_eq!(all.len(), 10);
        assert_eq!(all[1].0, "root/group/a_fan/0/0");
        let distinct = all
            .iter()
            .map(|(path, _)| path)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(distinct.len(), all.len());
        let executions: usize = store
            .db
            .query_row(
                "SELECT executions FROM workflows WHERE operation=?1",
                [&op],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(executions, all.len());
    }
}

#[test]
fn held_and_unsettled_stopping_children_keep_capacity_until_settled() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input =
        json!({"shape":"fan_out","items":["a","b"],"map_prompt":"{item}","max_concurrency":1});
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let child = children(&store, &op)[0].1.clone();
    store
        .db
        .execute("UPDATE threads SET held=1 WHERE id=?1", [&child])
        .unwrap();
    for _ in 0..3 {
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        assert_eq!(children(&store, &op).len(), 1);
    }
    let state: String = store
        .db
        .query_row(
            "SELECT state FROM workflow_nodes WHERE operation=?1 AND path='map/1'",
            [&op],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "waiting_capacity");
    let stopping = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'stopping')",
            params![stopping, child],
        )
        .unwrap();
    done(&mut store, &child, "cancelled", "effects stopping");
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    assert_eq!(children(&store, &op).len(), 1);
    store
        .db
        .execute("UPDATE runs SET state='cancelled' WHERE id=?1", [&stopping])
        .unwrap();
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let all = children(&store, &op);
    assert_eq!(all.len(), 2);
    done(&mut store, &all[1].1, "completed", "second completed");
    let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(output["output"]["outcomes"][1]["status"], "success");
}

#[test]
fn workflow_budget_is_scoped_but_default_still_obeys_global_ceiling() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let first =
        json!({"shape":"fan_out","items":["a","b"],"map_prompt":"{item}","max_concurrency":1});
    let (run_a, op_a) = operation(&store, &root, &first);
    assert!(tick(&mut store, &root, &run_a, &op_a, &models, &first).is_none());
    let op_b = sibling_operation(&store, &root, &run_a, &first);
    assert!(tick(&mut store, &root, &run_a, &op_b, &models, &first).is_none());
    assert_eq!(children(&store, &op_a).len(), 1);
    assert_eq!(children(&store, &op_b).len(), 1);
    let wide = json!({"shape":"fan_out","items":(0..70).map(|i|i.to_string()).collect::<Vec<_>>(),"map_prompt":"{item}"});
    let op_c = sibling_operation(&store, &root, &run_a, &wide);
    assert!(tick(&mut store, &root, &run_a, &op_c, &models, &wide).is_none());
    assert_eq!(children(&store, &op_c).len(), 62);
    let child = children(&store, &op_a)[0].1.clone();
    done(&mut store, &child, "completed", "one slot released");
    assert!(tick(&mut store, &root, &run_a, &op_c, &models, &wide).is_none());
    assert_eq!(children(&store, &op_c).len(), 63);
}

#[test]
fn cancellation_retains_choice_and_never_admits_capacity_waiters() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let mut input = conditional_input(false);
    input["steps"][1]["conditional"]["then"] =
        json!([{"id":"batch","fan_out":{"over":["a","b","c"],"prompt":"{item}"}}]);
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let check = children(&store, &op)[0].1.clone();
    done(&mut store, &check, "completed", r#"{"ok":true}"#);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let all = children(&store, &op);
    assert_eq!(all.len(), 2);
    let choice = node_result(&store, &op, "root/decision/!choice");
    let child_run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![child_run, all[1].1],
        )
        .unwrap();
    store
        .command(
            &Command::Stop {
                id: CommandId::new(),
                run: run.clone(),
            },
            &models,
        )
        .unwrap();
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    done(&mut store, &all[1].1, "cancelled", "stopping");
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    store
        .db
        .execute(
            "UPDATE runs SET state='cancelled' WHERE id=?1",
            [&child_run],
        )
        .unwrap();
    let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(children(&store, &op), all);
    assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
    let state: String = store
        .db
        .query_row(
            "SELECT state FROM workflows WHERE operation=?1",
            [&op],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "cancelled");
}

#[test]
fn choice_is_committed_before_any_selected_child_can_get_capacity() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"max_concurrency":1,"steps":[
        {"id":"check","agent":{"prompt":"Check","schema":{"type":"object"}}},
        {"id":"group","parallel":{"branches":{
            "a_busy":{"agent":{"prompt":"Occupy capacity"}},
            "z_choice":{"conditional":{"if":"$check.ok","then":[{"id":"yes","agent":{"prompt":"Chosen"}}]}}
        }}}
    ]});
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let check = children(&store, &op)[0].1.clone();
    done(&mut store, &check, "completed", r#"{"ok":true}"#);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    assert_eq!(children(&store, &op).len(), 2);
    let choice = node_result(&store, &op, "root/group/z_choice/!choice");
    let waiting: (String, Option<String>) = store.db.query_row(
        "SELECT state,child FROM workflow_nodes WHERE operation=?1 AND path='root/group/z_choice/then/yes'", [&op], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(waiting, ("waiting_capacity".into(), None));
    let changed = artifact(
        &store.db,
        &json!({"output":{"ok":false},"error":null}).to_string(),
    )
    .unwrap();
    store
        .db
        .execute(
            "UPDATE workflow_nodes SET result=?2 WHERE operation=?1 AND path='root/check'",
            params![op, changed],
        )
        .unwrap();
    let busy = children(&store, &op)[1].1.clone();
    done(&mut store, &busy, "completed", "slot released");
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    assert_eq!(children(&store, &op)[2].0, "root/group/z_choice/then/yes");
    assert_eq!(
        node_result(&store, &op, "root/group/z_choice/!choice"),
        choice
    );
}

#[test]
fn yielded_conditional_continuation_keeps_choice_budget_and_rechecks_permission() {
    for denied in [false, true] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let mut input = conditional_input(false);
        input["steps"][1]["conditional"]["then"] =
            json!([{"id":"batch","fan_out":{"over":["a","b"],"prompt":"{item}"}}]);
        let (run, op) = operation(&store, &root, &input);
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        let check = children(&store, &op)[0].1.clone();
        done(&mut store, &check, "completed", r#"{"ok":true}"#);
        assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
        let choice = node_result(&store, &op, "root/decision/!choice");
        let first = children(&store, &op)[1].1.clone();
        let tx = store.db.transaction().unwrap();
        tx.execute("UPDATE jobs SET background=1 WHERE operation=?1", [&op])
            .unwrap();
        store::finish_tool_entry(&tx, &op, r#"{"status":"running"}"#, false).unwrap();
        tx.execute("UPDATE operations SET state='done' WHERE id=?1", [&op])
            .unwrap();
        tx.commit().unwrap();
        let frozen: String = store
            .db
            .query_row("SELECT result FROM operations WHERE id=?1", [&op], |r| {
                r.get(0)
            })
            .unwrap();
        for _ in 0..3 {
            let tx = store.db.transaction().unwrap();
            super::cleanup(&tx, &root.session, &models).unwrap();
            tx.commit().unwrap();
            assert_eq!(children(&store, &op).len(), 2);
        }
        if denied {
            store
                .command(
                    &Command::SetPermissions {
                        id: CommandId::new(),
                        session: root.session.clone(),
                        expected_revision: 0,
                        permissions: crate::contracts::PermissionPolicy {
                            rules: vec![crate::contracts::PermissionRule {
                                tool: "workflow".into(),
                                decision: crate::contracts::RuleDecision::Deny,
                            }],
                            ..Default::default()
                        },
                    },
                    &models,
                )
                .unwrap();
        }
        done(&mut store, &first, "completed", "first settled");
        let tx = store.db.transaction().unwrap();
        super::cleanup(&tx, &root.session, &models).unwrap();
        tx.commit().unwrap();
        assert_eq!(children(&store, &op).len(), if denied { 2 } else { 3 });
        if !denied {
            let second = children(&store, &op)[2].1.clone();
            done(&mut store, &second, "completed", "second settled");
            let tx = store.db.transaction().unwrap();
            super::cleanup(&tx, &root.session, &models).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
        let current: String = store
            .db
            .query_row("SELECT result FROM operations WHERE id=?1", [&op], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(current, frozen);
        let state: String = store
            .db
            .query_row(
                "SELECT state FROM workflows WHERE operation=?1",
                [&op],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, if denied { "failed" } else { "completed" });
        let results: usize = store.db.query_row("SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind IN ('tool_result','tool_error')", [&op], |r| r.get(0)).unwrap();
        assert_eq!(results, 1);
    }
}

#[test]
fn conditional_choices_are_distinct_per_loop_iteration() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"max_concurrency":1,"steps":[{"id":"cycle","loop":{"max":2,"steps":[
        {"id":"check","agent":{"prompt":"Check this pass","schema":{"type":"object"}}},
        {"id":"decision","conditional":{"if":"$check.ok","then":[{"id":"selected","fan_out":{"over":[],"prompt":"{item}"}}]}}
    ]}}]});
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let first = children(&store, &op)[0].1.clone();
    done(&mut store, &first, "completed", r#"{"ok":true}"#);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let second = children(&store, &op)[1].1.clone();
    done(&mut store, &second, "completed", r#"{"ok":false}"#);
    let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    let first_choice = node_result(&store, &op, "root/cycle/pass0/decision/!choice");
    let second_choice = node_result(&store, &op, "root/cycle/pass1/decision/!choice");
    assert_ne!(first_choice.0, second_choice.0);
    assert_eq!(first_choice.1["output"]["branch"], "then");
    assert_eq!(second_choice.1["output"]["branch"], "else");
    assert_eq!(
        output["output"]["cycle"]["decision"],
        json!({"branch":"else","output":{}})
    );
}

#[test]
fn conditional_preserves_total_execution_budget() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = conditional_input(true);
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let check = children(&store, &op)[0].1.clone();
    done(&mut store, &check, "completed", r#"{"ok":true}"#);
    store
        .db
        .execute(
            "UPDATE workflows SET executions=1024 WHERE operation=?1",
            [&op],
        )
        .unwrap();
    let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert!(output["error"]
        .as_str()
        .unwrap()
        .contains("1024 child executions"));
    assert_eq!(children(&store, &op).len(), 1);
}

#[test]
fn reopen_keeps_committed_choice_and_never_replays_unknown_children() {
    let (dir, mut store, root, models) = store::tests::fixture();
    let input = conditional_input(true);
    let (run, op) = operation(&store, &root, &input);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let check = children(&store, &op)[0].1.clone();
    done(&mut store, &check, "completed", r#"{"ok":true}"#);
    assert!(tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let before = children(&store, &op);
    let choice = node_result(&store, &op, "root/decision/!choice");
    drop(store);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    assert_eq!(children(&store, &op), before);
    assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
    assert_eq!(pending(&store, &op).len(), 1);
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone()).unwrap();
    store.adopt_claim(claim, true).unwrap();
    assert!(pending(&store, &op).is_empty());
    assert_eq!(children(&store, &op), before);
    assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
    let output = tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(children(&store, &op), before);
    assert_eq!(node_result(&store, &op, "root/decision/!choice"), choice);
    assert_eq!(
        tick(&mut store, &root, &run, &op, &models, &input),
        Some(output)
    );
}
