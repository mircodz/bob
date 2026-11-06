use crate::effect_claims;
use crate::model::*;
use crate::store::tests::fixture;
use crate::store::Store;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn session(store: &mut Store, root: &Receipt, models: &[ModelInfo], path: &Path) -> Receipt {
    std::fs::create_dir_all(path).unwrap();
    let profile = crate::sessions::session(&store.db, &root.session)
        .unwrap()
        .profile;
    store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: path.to_string_lossy().into_owned(),
                profile,
                policy: Policy::default(),
            },
            models,
        )
        .unwrap()
}

fn tool(store: &Store, owner: &Receipt, input: serde_json::Value, writer: bool) -> PreparedTool {
    let run = store
        .db
        .query_row(
            "SELECT id FROM runs WHERE thread=?1",
            [&owner.thread],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
        .map(RunId)
        .unwrap_or_else(|| {
            let run = RunId::new();
            store
                .db
                .execute(
                    "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                    params![run, owner.thread],
                )
                .unwrap();
            run
        });
    let operation = OperationId::new();
    store.db.execute(
        "INSERT INTO operations(id,run,thread,name,kind,input,state,effect_write) VALUES(?1,?2,?3,'bash','tool',?4,'ready',?5)",
        params![operation, run, owner.thread, input.to_string(), writer],
    ).unwrap();
    PreparedTool {
        invocation: Invocation {
            operation_id: operation,
            run_id: run,
            thread_id: owner.thread.clone(),
            workspace: crate::sessions::session(&store.db, &owner.session)
                .unwrap()
                .workspace,
            name: "bash".into(),
            input,
        },
        specification: json!({}),
        preview: String::new(),
        presentation: None,
    }
}

fn resources(tool: &PreparedTool, targets: &[PathBuf]) -> Vec<PathBuf> {
    let requirements: Vec<_> = targets
        .iter()
        .cloned()
        .map(|path| PreparedAuthorizationRequirement::EditFile { path })
        .collect();
    effect_claims::resources(tool, &requirements).unwrap()
}

fn dispatch(
    tx: &Transaction<'_>,
    tool: &PreparedTool,
    owner: &Receipt,
    paths: &[PathBuf],
    writer: bool,
) -> anyhow::Result<bool> {
    let operation = &tool.invocation.operation_id;
    if !effect_claims::claim(
        tx,
        operation,
        &owner.session,
        "test-generation",
        paths,
        writer,
    )? {
        return Ok(false);
    }
    if tx.execute(
        "UPDATE operations SET state='started' WHERE id=?1 AND state='ready'",
        [operation],
    )? == 1
    {
        crate::store::change(
            tx,
            Some(&owner.thread),
            "effect_claim_test_dispatch",
            &operation.0,
        )?;
    }
    Ok(true)
}

fn attempt(
    store: &mut Store,
    tool: &PreparedTool,
    owner: &Receipt,
    paths: &[PathBuf],
    writer: bool,
) -> bool {
    let tx = store
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let claimed = dispatch(&tx, tool, owner, paths, writer).unwrap();
    tx.commit().unwrap();
    claimed
}

fn would_claim(store: &mut Store, tool: &PreparedTool, owner: &Receipt, writer: bool) -> bool {
    let tx = store
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let allowed = effect_claims::claim(
        &tx,
        &tool.invocation.operation_id,
        &owner.session,
        "probe-generation",
        &resources(tool, &[]),
        writer,
    )
    .unwrap();
    tx.rollback().unwrap();
    allowed
}

fn count(db: &Connection, tool: &PreparedTool) -> i64 {
    db.query_row(
        "SELECT COUNT(*) FROM effect_claims WHERE operation=?1",
        [&tool.invocation.operation_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn state(db: &Connection, tool: &PreparedTool, state: &str) {
    assert_eq!(
        db.execute(
            "UPDATE operations SET state=?2 WHERE id=?1",
            params![tool.invocation.operation_id, state]
        )
        .unwrap(),
        1
    );
}

fn shell_job(db: &Connection, tool: &PreparedTool, owner: &Receipt, state: &str) -> JobId {
    let job = JobId::new();
    db.execute(
        "INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) VALUES(?1,?2,?3,?4,?5,'shell',?6,'claim fixture',1,0)",
        params![job, owner.session, owner.thread, tool.invocation.operation_id, tool.invocation.run_id, state],
    ).unwrap();
    job
}

fn job_state(db: &Connection, job: &JobId, state: &str) {
    assert_eq!(
        db.execute("UPDATE jobs SET state=?2 WHERE id=?1", params![job, state])
            .unwrap(),
        1
    );
}

#[test]
fn multi_target_conflict_leaves_no_partial_claims_or_dispatch() {
    let (dir, mut store, root, models) = fixture();
    let first = session(&mut store, &root, &models, &dir.0.join("z-busy"));
    let second = session(&mut store, &root, &models, &dir.0.join("a-free"));
    let blocker = tool(&store, &first, json!({}), true);
    let blocked = tool(&store, &second, json!({}), true);
    let blocked_paths = resources(
        &blocked,
        &[
            dir.0.join("b-external/new.txt"),
            dir.0.join("z-busy/new.txt"),
        ],
    );
    assert_eq!(blocked_paths.len(), 3);
    assert!(attempt(
        &mut store,
        &blocker,
        &first,
        &resources(&blocker, &[]),
        true
    ));
    assert!(!attempt(
        &mut store,
        &blocked,
        &second,
        &blocked_paths,
        true
    ));
    assert_eq!(count(&store.db, &blocked), 0);
    let unchanged: String = store
        .db
        .query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&blocked.invocation.operation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, "ready");
    state(&store.db, &blocker, "done");
    assert!(attempt(&mut store, &blocked, &second, &blocked_paths, true));
    assert_eq!(count(&store.db, &blocked), 3);
    let metadata: i64 = store.db.query_row(
        "SELECT COUNT(*) FROM effect_claims WHERE operation=?1 AND session=?2 AND generation='test-generation' AND writer=1",
        params![blocked.invocation.operation_id, second.session], |row| row.get(0),
    ).unwrap();
    assert_eq!(metadata, 3);
}

#[test]
fn later_claim_insert_failure_rolls_back_all_targets() {
    let (dir, mut store, root, _) = fixture();
    let prepared = tool(&store, &root, json!({}), true);
    let paths = resources(&prepared, &[dir.0.join("a.txt"), dir.0.join("z.txt")]);
    store.db.execute_batch("CREATE TRIGGER reject_later_claim BEFORE INSERT ON effect_claims WHEN EXISTS(SELECT 1 FROM effect_claims WHERE operation=NEW.operation) BEGIN SELECT RAISE(ABORT,'injected later claim failure'); END;").unwrap();
    {
        let tx = store
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let error = dispatch(&tx, &prepared, &root, &paths, true).unwrap_err();
        assert!(error.to_string().contains("injected later claim failure"));
        assert_eq!(
            count(&tx, &prepared),
            1,
            "failure must occur after the first insert"
        );
        tx.rollback().unwrap();
    }
    assert_eq!(count(&store.db, &prepared), 0);
    store
        .db
        .execute_batch("DROP TRIGGER reject_later_claim")
        .unwrap();
    assert!(attempt(&mut store, &prepared, &root, &paths, true));
    assert_eq!(count(&store.db, &prepared), 3);
}

#[test]
fn dispatch_failure_rolls_back_claims_and_publication_together() {
    let (dir, mut store, root, _) = fixture();
    let prepared = tool(&store, &root, json!({}), true);
    let paths = resources(&prepared, &[dir.0.join("new.txt")]);
    store.db.execute_batch("CREATE TRIGGER reject_claim_publication BEFORE INSERT ON changes WHEN NEW.kind='effect_claim_test_dispatch' BEGIN SELECT RAISE(ABORT,'injected dispatch failure'); END;").unwrap();
    {
        let tx = store
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let error = dispatch(&tx, &prepared, &root, &paths, true).unwrap_err();
        assert!(error.to_string().contains("injected dispatch failure"));
        tx.rollback().unwrap();
    }
    assert_eq!(count(&store.db, &prepared), 0);
    let unchanged: String = store
        .db
        .query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&prepared.invocation.operation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, "ready");
    store
        .db
        .execute_batch("DROP TRIGGER reject_claim_publication")
        .unwrap();
    assert!(attempt(&mut store, &prepared, &root, &paths, true));
    assert_eq!(count(&store.db, &prepared), 2);
}

#[test]
fn overlapping_roots_conflict_in_both_directions_but_component_siblings_do_not() {
    for child_first in [false, true] {
        let (dir, mut store, root, models) = fixture();
        let parent = session(&mut store, &root, &models, &dir.0.join("project"));
        let child = session(&mut store, &root, &models, &dir.0.join("project/nested"));
        let sibling = session(&mut store, &root, &models, &dir.0.join("project-other"));
        let (first, second) = if child_first {
            (&child, &parent)
        } else {
            (&parent, &child)
        };
        let active = tool(&store, first, json!({}), true);
        let blocked = tool(&store, second, json!({}), false);
        let independent = tool(&store, &sibling, json!({}), true);
        assert!(attempt(
            &mut store,
            &active,
            first,
            &resources(&active, &[]),
            true
        ));
        assert!(!attempt(
            &mut store,
            &blocked,
            second,
            &resources(&blocked, &[]),
            false
        ));
        assert!(attempt(
            &mut store,
            &independent,
            &sibling,
            &resources(&independent, &[]),
            true
        ));
        assert_eq!(count(&store.db, &blocked), 0);
        let active_count: i64 = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE state='started'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_count, 2);
    }
}

#[test]
fn canonical_dot_aliases_and_authorization_targets_are_deduplicated() {
    let (dir, mut store, root, models) = fixture();
    let owner = session(&mut store, &root, &models, &dir.0.join("project"));
    std::fs::create_dir(dir.0.join("project/nested")).unwrap();
    let mut prepared = tool(&store, &owner, json!({}), true);
    prepared.invocation.workspace = dir
        .0
        .join("project/nested/../.")
        .to_string_lossy()
        .into_owned();
    let paths = effect_claims::resources(
        &prepared,
        &[
            PreparedAuthorizationRequirement::ToolPath {
                path: PathBuf::from("."),
            },
            PreparedAuthorizationRequirement::EditFile {
                path: PathBuf::from("new/file.txt"),
            },
            PreparedAuthorizationRequirement::ToolPath {
                path: dir.0.join("project/new/file.txt"),
            },
        ],
    )
    .unwrap();
    let canonical = std::fs::canonicalize(dir.0.join("project")).unwrap();
    assert_eq!(
        paths,
        vec![canonical.clone(), canonical.join("new/file.txt")]
    );
    assert!(attempt(&mut store, &prepared, &owner, &paths, true));
    let contender = session(&mut store, &root, &models, &dir.0.join("elsewhere"));
    let other = tool(&store, &contender, json!({}), true);
    assert!(!attempt(
        &mut store,
        &other,
        &contender,
        &resources(&other, &[dir.0.join("project/new/file.txt")]),
        true
    ));
}

#[cfg(unix)]
#[test]
fn symlink_workspace_and_missing_target_aliases_cannot_bypass_claims() {
    let (dir, mut store, root, models) = fixture();
    let owner = session(&mut store, &root, &models, &dir.0.join("project"));
    let alias = dir.0.join("alias");
    std::os::unix::fs::symlink(dir.0.join("project"), &alias).unwrap();
    let contender = session(&mut store, &root, &models, &dir.0.join("elsewhere"));
    let active = tool(&store, &owner, json!({}), true);
    assert!(attempt(
        &mut store,
        &active,
        &owner,
        &resources(&active, &[]),
        true
    ));
    let mut aliased = tool(&store, &contender, json!({}), true);
    assert!(!attempt(
        &mut store,
        &aliased,
        &contender,
        &resources(&aliased, &[alias.join("new/file.txt")]),
        true
    ));
    aliased.invocation.workspace = alias.to_string_lossy().into_owned();
    assert_eq!(resources(&aliased, &[]), resources(&active, &[]));
    assert!(!attempt(
        &mut store,
        &aliased,
        &contender,
        &resources(&aliased, &[]),
        true
    ));
    assert_eq!(count(&store.db, &aliased), 0);
}

#[test]
fn overlapping_readers_can_run_but_a_writer_conflicts_with_either_effect_class() {
    for first_writer in [false, true] {
        for second_writer in [false, true] {
            let (dir, mut store, root, models) = fixture();
            let other = session(&mut store, &root, &models, &dir.0);
            let first = tool(&store, &root, json!({}), first_writer);
            let second = tool(&store, &other, json!({}), second_writer);
            assert!(attempt(
                &mut store,
                &first,
                &root,
                &resources(&first, &[]),
                first_writer
            ));
            let allowed = !first_writer && !second_writer;
            assert_eq!(
                attempt(
                    &mut store,
                    &second,
                    &other,
                    &resources(&second, &[]),
                    second_writer
                ),
                allowed
            );
            assert_eq!(count(&store.db, &second), i64::from(allowed));
        }
    }
}

#[test]
fn only_started_operations_keep_claims_active() {
    let (dir, mut store, root, models) = fixture();
    let other = session(&mut store, &root, &models, &dir.0);
    let active = tool(&store, &root, json!({}), true);
    let contender = tool(&store, &other, json!({}), true);
    assert!(attempt(
        &mut store,
        &active,
        &root,
        &resources(&active, &[]),
        true
    ));
    for operation_state in [
        "pending",
        "preparing",
        "approval",
        "ready",
        "waiting",
        "started",
        "unknown",
        "done",
        "failed",
        "cancelled",
        "interrupted",
    ] {
        state(&store.db, &active, operation_state);
        assert_eq!(
            would_claim(&mut store, &contender, &other, true),
            operation_state != "started",
            "{operation_state}"
        );
    }
}

#[test]
fn claims_of_a_session_whose_process_died_do_not_block() {
    let (dir, mut store, root, models) = fixture();
    let other = session(&mut store, &root, &models, &dir.0);
    let owner =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone()).unwrap();
    let active = tool(&store, &root, json!({}), true);
    let contender = tool(&store, &other, json!({}), false);
    assert!(attempt(
        &mut store,
        &active,
        &root,
        &resources(&active, &[]),
        true
    ));
    assert!(!would_claim(&mut store, &contender, &other, false));
    drop(owner);
    assert!(would_claim(&mut store, &contender, &other, false));
    let local = tool(&store, &root, json!({}), true);
    assert!(
        !would_claim(&mut store, &local, &root, true),
        "a session's own in-flight effect still serializes its writes"
    );
}

#[test]
fn background_shell_jobs_do_not_block_other_sessions() {
    for explicit in [false, true] {
        let (dir, mut store, root, models) = fixture();
        let other = session(&mut store, &root, &models, &dir.0);
        let active = tool(&store, &root, json!({"run_in_background": explicit}), true);
        let contender = tool(&store, &other, json!({}), true);
        assert!(attempt(
            &mut store,
            &active,
            &root,
            &resources(&active, &[]),
            true
        ));
        let job = shell_job(&store.db, &active, &root, "queued");
        state(&store.db, &active, "done");
        for live in ["queued", "running", "stopping", "unknown"] {
            job_state(&store.db, &job, live);
            assert!(
                would_claim(&mut store, &contender, &other, true),
                "explicit={explicit}, job={live}"
            );
        }
    }
}

#[test]
fn explicit_background_unknown_preserves_acknowledgement_without_blocking_peers() {
    for completion in ["interrupted", "error", "recovery"] {
        let (dir, mut store, root, models) = fixture();
        let other = session(&mut store, &root, &models, &dir.0);
        let active = tool(&store, &root, json!({"run_in_background": true}), true);
        let contender = tool(&store, &other, json!({}), true);
        let local = tool(&store, &root, json!({}), true);
        let peer_job = shell_job(&store.db, &contender, &other, "running");
        let operation = &active.invocation.operation_id;
        store
            .db
            .execute("UPDATE operations SET call_id=id WHERE id=?1", [operation])
            .unwrap();
        assert!(attempt(
            &mut store,
            &active,
            &root,
            &resources(&active, &[]),
            true
        ));
        let tx = store.db.transaction().unwrap();
        let job = crate::jobs::start_shell(&tx, &active).unwrap();
        tx.commit().unwrap();
        let acknowledged: String = store
            .db
            .query_row(
                "SELECT result FROM operations WHERE id=?1",
                [operation],
                |r| r.get(0),
            )
            .unwrap();
        assert!(store
            .command(
                &Command::ResolveUnknown {
                    id: CommandId::new(),
                    operation: operation.clone(),
                    evidence: "A running job is not an unknown outcome".into(),
                },
                &models
            )
            .is_err());
        if completion == "recovery" {
            store
                .db
                .execute(
                    "UPDATE runs SET state='completed' WHERE id=?1",
                    [&active.invocation.run_id],
                )
                .unwrap();
            drop(store);
            store = Store::open(&dir.0.join("lab.db")).unwrap();
            assert_eq!(
                store
                    .db
                    .query_row("SELECT state FROM jobs WHERE id=?1", [&job], |r| r
                        .get::<_, String>(0),)
                    .unwrap(),
                "running"
            );
            let claim =
                crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone())
                    .unwrap();
            store.adopt_claim(claim, true).unwrap();
        } else {
            let result = if completion == "error" {
                Err(anyhow::anyhow!("executor lost completion"))
            } else {
                Ok((
                    ToolOutcome {
                        text: "interrupted background output".into(),
                        is_error: false,
                        interrupted: true,
                        presentation: None,
                    },
                    None,
                ))
            };
            let tx = store.db.transaction().unwrap();
            crate::jobs::finish_shell(&tx, operation, result).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            store
                .db
                .query_row("SELECT state FROM jobs WHERE id=?1", [&job], |r| r
                    .get::<_, String>(0),)
                .unwrap(),
            "unknown"
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT state FROM jobs WHERE id=?1", [&peer_job], |r| {
                    r.get::<_, String>(0)
                },)
                .unwrap(),
            "running"
        );
        assert!(!store
            .db
            .query_row(
                "SELECT held FROM threads WHERE id=?1",
                [&root.thread],
                |r| r.get::<_, bool>(0),
            )
            .unwrap());
        assert_eq!(count(&store.db, &active), 1);
        assert!(would_claim(&mut store, &contender, &other, true));
        let tx = store.db.transaction().unwrap();
        assert!(effect_claims::claim(
            &tx,
            &local.invocation.operation_id,
            &root.session,
            "same-session",
            &resources(&local, &[]),
            true
        )
        .unwrap());
        tx.rollback().unwrap();
        assert!(store
            .command(
                &Command::ResolveUnknown {
                    id: CommandId::new(),
                    operation: operation.clone(),
                    evidence: " ".into(),
                },
                &models
            )
            .is_err());
        assert_eq!(count(&store.db, &active), 1);
        store
            .command(
                &Command::ResolveUnknown {
                    id: CommandId::new(),
                    operation: operation.clone(),
                    evidence: "Inspected the job effects and verified that the process terminated"
                        .into(),
                },
                &models,
            )
            .unwrap();
        assert_eq!(count(&store.db, &active), 0);
        assert_eq!(
            store
                .db
                .query_row("SELECT state FROM jobs WHERE id=?1", [&job], |r| r
                    .get::<_, String>(0),)
                .unwrap(),
            "cancelled"
        );
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT state,result FROM operations WHERE id=?1",
                    [operation],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .unwrap(),
            ("done".into(), acknowledged)
        );
        assert_eq!(store.db.query_row(
            "SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind IN ('tool_result','tool_error')",
            [operation], |r| r.get::<_, usize>(0),
        ).unwrap(), 1);
        assert!(attempt(
            &mut store,
            &contender,
            &other,
            &resources(&contender, &[]),
            true
        ));
    }
}

#[test]
fn non_shell_jobs_do_not_extend_finished_operation_claims() {
    for kind in ["task", "workflow", "turn"] {
        let (dir, mut store, root, models) = fixture();
        let other = session(&mut store, &root, &models, &dir.0);
        let active = tool(&store, &root, json!({}), true);
        let contender = tool(&store, &other, json!({}), true);
        assert!(attempt(
            &mut store,
            &active,
            &root,
            &resources(&active, &[]),
            true
        ));
        let job = shell_job(&store.db, &active, &root, "unknown");
        store
            .db
            .execute("UPDATE jobs SET kind=?2 WHERE id=?1", params![job, kind])
            .unwrap();
        state(&store.db, &active, "done");
        assert!(
            attempt(
                &mut store,
                &contender,
                &other,
                &resources(&contender, &[]),
                true
            ),
            "{kind}"
        );
    }
}

#[test]
fn unknown_operations_and_jobs_do_not_block_peers() {
    for operation_state in ["unknown", "done"] {
        let (dir, mut store, root, models) = fixture();
        let other = session(&mut store, &root, &models, &dir.0);
        let active = tool(&store, &root, json!({"run_in_background": true}), true);
        let contender = tool(&store, &other, json!({}), true);
        assert!(attempt(
            &mut store,
            &active,
            &root,
            &resources(&active, &[]),
            true
        ));
        state(&store.db, &active, operation_state);
        shell_job(&store.db, &active, &root, "unknown");
        assert!(would_claim(&mut store, &contender, &other, true));
        assert!(would_claim(&mut store, &contender, &other, false));
    }
}

#[test]
fn same_session_claims_only_block_on_started_operations() {
    for input in [
        json!({"run_in_background": true}),
        json!({"run_in_background": false}),
        json!({}),
    ] {
        let (_dir, mut store, root, _) = fixture();
        let active = tool(&store, &root, input, true);
        let contender = tool(&store, &root, json!({}), true);
        assert!(attempt(
            &mut store,
            &active,
            &root,
            &resources(&active, &[]),
            true
        ));
        let job = shell_job(&store.db, &active, &root, "running");
        for operation_state in ["started", "unknown", "done"] {
            state(&store.db, &active, operation_state);
            for live in ["queued", "running", "stopping", "unknown"] {
                job_state(&store.db, &job, live);
                assert_eq!(
                    would_claim(&mut store, &contender, &root, true),
                    operation_state != "started",
                    "operation={operation_state}, job={live}"
                );
            }
        }
    }
}

#[test]
fn settled_claim_release_is_transactional_and_preserves_uncertain_effects() {
    let (_dir, mut store, root, _) = fixture();
    let active = tool(&store, &root, json!({}), true);
    assert!(attempt(
        &mut store,
        &active,
        &root,
        &resources(&active, &[]),
        true
    ));
    for operation_state in [
        "started",
        "unknown",
        "done",
        "failed",
        "cancelled",
        "resolved",
    ] {
        state(&store.db, &active, operation_state);
        let tx = store.db.transaction().unwrap();
        effect_claims::release_settled(&tx, &active.invocation.operation_id).unwrap();
        assert_eq!(
            count(&tx, &active),
            i64::from(matches!(operation_state, "started" | "unknown"))
        );
        tx.rollback().unwrap();
        assert_eq!(count(&store.db, &active), 1);
    }
    let tx = store.db.transaction().unwrap();
    effect_claims::release_settled(&tx, &active.invocation.operation_id).unwrap();
    tx.commit().unwrap();
    assert_eq!(count(&store.db, &active), 0);
}

#[test]
fn settled_claim_release_preserves_live_and_unknown_background_jobs() {
    let (_dir, mut store, root, models) = fixture();
    let active = tool(&store, &root, json!({"run_in_background": true}), true);
    assert!(attempt(
        &mut store,
        &active,
        &root,
        &resources(&active, &[]),
        true
    ));
    let job = shell_job(&store.db, &active, &root, "running");
    state(&store.db, &active, "done");
    for job_status in ["queued", "running", "stopping", "unknown"] {
        job_state(&store.db, &job, job_status);
        let tx = store.db.transaction().unwrap();
        effect_claims::release_settled(&tx, &active.invocation.operation_id).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&store.db, &active), 1, "{job_status}");
    }
    state(&store.db, &active, "unknown");
    store
        .command(
            &Command::ResolveUnknown {
                id: CommandId::new(),
                operation: active.invocation.operation_id.clone(),
                evidence: "Fixture verifies the process has terminated".into(),
            },
            &models,
        )
        .unwrap();
    assert_eq!(count(&store.db, &active), 0);
}

#[test]
fn immediate_transaction_contention_has_no_partial_claims_and_retry_publishes_once() {
    let (dir, mut first_store, root, models) = fixture();
    let other = session(&mut first_store, &root, &models, &dir.0);
    let first = tool(&first_store, &root, json!({}), true);
    let second = tool(&first_store, &other, json!({}), true);
    let paths = resources(&first, &[dir.0.join("a.txt"), dir.0.join("b.txt")]);
    let mut second_store = Store::open(&dir.0.join("lab.db")).unwrap();
    second_store.db.busy_timeout(Duration::ZERO).unwrap();
    let tx = first_store
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(dispatch(&tx, &first, &root, &paths, true).unwrap());
    assert_eq!(count(&tx, &first), 3);
    assert_eq!(
        count(&second_store.db, &first),
        0,
        "uncommitted claims must not leak"
    );
    let error = match second_store
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
    {
        Ok(_) => panic!("second connection acquired the held writer transaction"),
        Err(error) => error,
    };
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert_eq!(count(&second_store.db, &second), 0);
    tx.commit().unwrap();
    assert_eq!(count(&second_store.db, &first), 3);
    assert!(!attempt(&mut second_store, &second, &other, &paths, true));
    assert_eq!(count(&second_store.db, &second), 0);
    state(&first_store.db, &first, "done");
    assert!(attempt(&mut second_store, &second, &other, &paths, true));
    assert!(attempt(&mut second_store, &second, &other, &paths, true));
    assert_eq!(
        count(&first_store.db, &second),
        3,
        "retry must upsert rather than duplicate claims"
    );
    for prepared in [&first, &second] {
        let publications: i64 = first_store.db.query_row(
            "SELECT COUNT(*) FROM changes WHERE kind='effect_claim_test_dispatch' AND reference=?1",
            [&prepared.invocation.operation_id], |row| row.get(0),
        ).unwrap();
        assert_eq!(publications, 1);
    }
}
