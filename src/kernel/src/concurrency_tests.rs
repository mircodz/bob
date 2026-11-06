use crate::store::tests::{fixture, TempDir};
use crate::store::Store;
use crate::*;
use async_trait::async_trait;
use bob_types::{Completion, ContentBlock, Message, Role, StopReason};
use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tokio::sync::{watch, Semaphore};

struct GatedExecutor {
    models: Vec<ModelInfo>,
    entered: Semaphore,
    release: Semaphore,
    returned: Semaphore,
    calls: AtomicUsize,
    cancellations: AtomicUsize,
    ignore_cancel: bool,
}

impl GatedExecutor {
    fn new(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            returned: Semaphore::new(0),
            calls: AtomicUsize::new(0),
            cancellations: AtomicUsize::new(0),
            ignore_cancel: false,
        }
    }

    async fn started(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.acquire())
            .await
            .expect("model did not reach its execution gate")
            .unwrap()
            .forget();
    }

    async fn assert_not_started(&self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(150), self.entered.acquire())
                .await
                .is_err(),
            "unclaimed or paused runtime dispatched a model"
        );
        assert_eq!(self.calls.load(Ordering::SeqCst), 0);
    }
}

#[async_trait]
impl Executor for GatedExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }

    async fn generate(
        &self,
        _: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _: ModelProgress,
    ) -> anyhow::Result<Completion> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        tokio::select! {
            permit = self.release.acquire() => permit.unwrap().forget(),
            _ = crate::cancelled(&mut cancel), if !self.ignore_cancel => {
                self.cancellations.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("fixture model cancelled");
            }
        }
        self.returned.add_permits(1);
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "gated answer".into(),
                }],
            },
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
        })
    }

    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        anyhow::bail!("unexpected tool preparation")
    }

    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        anyhow::bail!("unexpected tool execution")
    }
}

fn sibling_command(store: &Store, root: &Receipt) -> Command {
    let session = crate::sessions::session(&store.db, &root.session).unwrap();
    Command::CreateSession {
        id: CommandId::new(),
        workspace: session.workspace,
        profile: session.profile,
        policy: Policy::default(),
    }
}

fn sibling(store: &mut Store, root: &Receipt, models: &[ModelInfo]) -> Receipt {
    store
        .command(&sibling_command(store, root), models)
        .unwrap()
}

fn submit(root: &Receipt, text: &str) -> Command {
    Command::Submit {
        id: CommandId::new(),
        thread: root.thread.clone(),
        text: text.into(),
    }
}

fn reader(path: &Path) -> Connection {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db
}

fn thread_rows(db: &Connection, sql: &str, thread: &ThreadId) -> Vec<Vec<rusqlite::types::Value>> {
    let mut statement = db.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([thread], |row| {
            (0..columns).map(|column| row.get(column)).collect()
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn thread_snapshot(db: &Connection, thread: &ThreadId) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    let mut snapshot = vec![thread_rows(db, "SELECT * FROM threads WHERE id=?1", thread)];
    for table in [
        "runs",
        "operations",
        "submissions",
        "entries",
        "jobs",
        "usage",
        "changes",
        "contexts",
        "working",
    ] {
        snapshot.push(thread_rows(
            db,
            &format!("SELECT * FROM {table} WHERE thread=?1 ORDER BY rowid"),
            thread,
        ));
    }
    snapshot
}

fn abandoned_model(store: &Store, root: &Receipt) -> (RunId, OperationId) {
    let run = RunId::new();
    let operation = OperationId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, root.thread],
        )
        .unwrap();
    store
        .db
        .execute(
            "INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'model','model','{}','started')",
            params![operation, run, root.thread],
        )
        .unwrap();
    (run, operation)
}

async fn completed(handle: &Handle, root: &Receipt) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .view(Some(root.thread.clone()), None, 100)
                .await
                .unwrap();
            assert!(view.error.is_none(), "kernel halted: {:?}", view.error);
            if view
                .threads
                .iter()
                .any(|thread| thread.id == root.thread && thread.state == ThreadState::Completed)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gated model did not complete");
}

async fn shutdown(handle: &Handle) {
    tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
        .await
        .expect("kernel did not shut down")
        .unwrap();
}

#[tokio::test]
async fn distinct_sessions_in_one_database_have_simultaneous_model_requests() {
    let (dir, mut store, first, models) = fixture();
    let second = sibling(&mut store, &first, &models);
    let unclaimed = sibling(&mut store, &first, &models);
    store
        .command(&submit(&unclaimed, "leave this session queued"), &models)
        .unwrap();
    let unclaimed_before = thread_snapshot(&store.db, &unclaimed.thread);
    drop(store);
    let path = dir.0.join("lab.db");
    let first_executor = Arc::new(GatedExecutor::new(models.clone()));
    let second_executor = Arc::new(GatedExecutor::new(models));
    let first_handle = Runtime::open(&path, first_executor.clone()).await.unwrap();
    let second_handle = Runtime::open(&path, second_executor.clone()).await.unwrap();
    let (first_activation, second_activation) = tokio::join!(
        first_handle.activate_session(first.session.clone(), vec![submit(&first, "first work")]),
        second_handle
            .activate_session(second.session.clone(), vec![submit(&second, "second work")]),
    );
    first_activation.unwrap();
    second_activation.unwrap();
    tokio::join!(first_executor.started(), second_executor.started());

    let db = reader(&path);
    let active: i64 = db
        .query_row(
            "SELECT COUNT(DISTINCT t.session) FROM operations o JOIN threads t ON t.id=o.thread WHERE o.kind='model' AND o.state='started'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        active, 2,
        "both requests must be in flight before either release"
    );
    assert_eq!(first_executor.cancellations.load(Ordering::SeqCst), 0);
    assert_eq!(second_executor.cancellations.load(Ordering::SeqCst), 0);
    assert!(first_handle
        .command(submit(&second, "wrong owner"))
        .await
        .is_err());
    assert!(second_handle
        .command(submit(&first, "wrong owner"))
        .await
        .is_err());

    first_executor.release.add_permits(1);
    second_executor.release.add_permits(1);
    tokio::join!(
        completed(&first_handle, &first),
        completed(&second_handle, &second)
    );
    assert_eq!(first_executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(thread_snapshot(&db, &unclaimed.thread), unclaimed_before);
    tokio::join!(shutdown(&first_handle), shutdown(&second_handle));
    assert_eq!(thread_snapshot(&db, &unclaimed.thread), unclaimed_before);
}

#[test]
fn opening_store_does_not_recover_abandoned_session_work() {
    let (dir, store, root, _) = fixture();
    abandoned_model(&store, &root);
    let before = thread_snapshot(&store.db, &root.thread);
    drop(store);
    let first = Store::open(&dir.0.join("lab.db")).unwrap();
    let second = Store::open(&dir.0.join("lab.db")).unwrap();
    assert_eq!(thread_snapshot(&first.db, &root.thread), before);
    assert_eq!(thread_snapshot(&second.db, &root.thread), before);
}

#[tokio::test]
async fn unclaimed_runtimes_leave_queued_and_abandoned_sessions_untouched() {
    let (dir, mut store, queued, models) = fixture();
    let abandoned = sibling(&mut store, &queued, &models);
    store
        .command(&submit(&queued, "unclaimed input"), &models)
        .unwrap();
    abandoned_model(&store, &abandoned);
    let queued_before = thread_snapshot(&store.db, &queued.thread);
    let abandoned_before = thread_snapshot(&store.db, &abandoned.thread);
    drop(store);
    let path = dir.0.join("lab.db");
    let executor = Arc::new(GatedExecutor::new(models));
    let ordinary = Runtime::open(&path, executor.clone()).await.unwrap();
    let paused = Runtime::open_paused(&path, executor.clone()).await.unwrap();
    for handle in [&ordinary, &paused] {
        assert!(handle
            .command(submit(&queued, "must reject unclaimed mutation"))
            .await
            .is_err());
        assert_eq!(handle.queue(queued.thread.clone()).await.unwrap().len(), 1);
    }
    executor.assert_not_started().await;
    tokio::join!(shutdown(&ordinary), shutdown(&paused));
    let db = reader(&path);
    assert_eq!(thread_snapshot(&db, &queued.thread), queued_before);
    assert_eq!(thread_snapshot(&db, &abandoned.thread), abandoned_before);
}

#[tokio::test]
async fn first_create_claims_and_selects_session_preserving_requested_pause_mode() {
    for paused in [false, true] {
        let (dir, store, untouched, models) = fixture();
        let create = sibling_command(&store, &untouched);
        let before = thread_snapshot(&store.db, &untouched.thread);
        drop(store);
        let path = dir.0.join("lab.db");
        let executor = Arc::new(GatedExecutor::new(models));
        let handle = if paused {
            Runtime::open_paused(&path, executor.clone()).await.unwrap()
        } else {
            Runtime::open(&path, executor.clone()).await.unwrap()
        };
        let created = handle.command(create).await.unwrap();
        assert!(SessionClaim::acquire(&path, created.session.clone()).is_err());
        assert!(handle
            .command(submit(&untouched, "inactive session"))
            .await
            .is_err());
        handle
            .command(submit(&created, "new session work"))
            .await
            .unwrap();
        if paused {
            executor.assert_not_started().await;
            assert_eq!(handle.queue(created.thread.clone()).await.unwrap().len(), 1);
            handle
                .activate_session(created.session.clone(), vec![])
                .await
                .unwrap();
        }
        executor.started().await;
        executor.release.add_permits(1);
        completed(&handle, &created).await;
        assert_eq!(thread_snapshot(&reader(&path), &untouched.thread), before);
        shutdown(&handle).await;
        assert!(SessionClaim::acquire(&path, created.session).is_ok());
    }
}

#[tokio::test]
async fn concurrent_create_retries_have_one_session_and_one_owner() {
    let (dir, store, root, models) = fixture();
    let create = sibling_command(&store, &root);
    drop(store);
    let path = dir.0.join("lab.db");
    let executor = Arc::new(GatedExecutor::new(models));
    let first = Runtime::open(&path, executor.clone()).await.unwrap();
    let second = Runtime::open(&path, executor.clone()).await.unwrap();
    let (first_result, second_result) = tokio::join!(
        first.command(create.clone()),
        second.command(create.clone()),
    );
    let (owner, contender, created) = match (first_result, second_result) {
        (Ok(created), Err(_)) => (&first, &second, created),
        (Err(_), Ok(created)) => (&second, &first, created),
        results => panic!("expected one successful creator and one owner conflict: {results:?}"),
    };
    assert_eq!(
        reader(&path)
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2,
        "retrying one create command must not create another session"
    );
    let replay = owner.command(create).await.unwrap();
    assert_eq!(replay.session, created.session);
    assert_eq!(replay.thread, created.thread);
    assert_eq!(replay.sequence, created.sequence);
    assert!(SessionClaim::acquire(&path, created.session.clone()).is_err());
    assert!(contender
        .command(submit(&created, "unowned retry"))
        .await
        .is_err());
    assert!(contender
        .activate_session(created.session.clone(), vec![])
        .await
        .is_err());
    shutdown(owner).await;
    contender
        .activate_session(created.session, vec![])
        .await
        .unwrap();
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    shutdown(contender).await;
}

#[tokio::test]
async fn claimed_open_stays_paused_and_conflicting_activation_has_no_side_effects() {
    let (dir, mut store, root, models) = fixture();
    store
        .command(&submit(&root, "already queued"), &models)
        .unwrap();
    let before = thread_snapshot(&store.db, &root.thread);
    drop(store);
    let path = dir.0.join("lab.db");
    let executor = Arc::new(GatedExecutor::new(models));
    let claim = SessionClaim::acquire(&path, root.session.clone()).unwrap();
    let owner = Runtime::open_claimed_with_options(
        &path,
        executor.clone(),
        RuntimeOptions::default(),
        claim,
    )
    .await
    .unwrap();
    let contender = Runtime::open(&path, executor.clone()).await.unwrap();
    assert!(contender
        .activate_session(
            root.session.clone(),
            vec![submit(&root, "rejected bootstrap")]
        )
        .await
        .is_err());
    assert!(SessionClaim::acquire(&path, root.session.clone()).is_err());
    executor.assert_not_started().await;
    assert_eq!(thread_snapshot(&reader(&path), &root.thread), before);

    shutdown(&owner).await;
    contender
        .activate_session(root.session.clone(), vec![])
        .await
        .unwrap();
    executor.started().await;
    executor.release.add_permits(1);
    completed(&contender, &root).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    shutdown(&contender).await;
}

#[tokio::test]
async fn closing_one_session_and_unclaimed_observer_leaves_live_peer_untouched() {
    let (dir, mut store, first, models) = fixture();
    let second = sibling(&mut store, &first, &models);
    drop(store);
    let path = dir.0.join("lab.db");
    let first_executor = Arc::new(GatedExecutor::new(models.clone()));
    let second_executor = Arc::new(GatedExecutor::new(models.clone()));
    let first_handle = Runtime::open(&path, first_executor.clone()).await.unwrap();
    let second_handle = Runtime::open(&path, second_executor.clone()).await.unwrap();
    first_handle
        .activate_session(first.session.clone(), vec![submit(&first, "cancel me")])
        .await
        .unwrap();
    second_handle
        .activate_session(
            second.session.clone(),
            vec![submit(&second, "keep running")],
        )
        .await
        .unwrap();
    tokio::join!(first_executor.started(), second_executor.started());
    let db = reader(&path);
    let before = thread_snapshot(&db, &second.thread);
    shutdown(&first_handle).await;
    assert_eq!(first_executor.cancellations.load(Ordering::SeqCst), 1);
    assert_eq!(second_executor.cancellations.load(Ordering::SeqCst), 0);
    assert_eq!(thread_snapshot(&db, &second.thread), before);
    assert!(SessionClaim::acquire(&path, first.session).is_ok());
    assert!(SessionClaim::acquire(&path, second.session.clone()).is_err());

    let observer = Runtime::open(&path, Arc::new(GatedExecutor::new(models)))
        .await
        .unwrap();
    shutdown(&observer).await;
    assert_eq!(thread_snapshot(&db, &second.thread), before);
    second_executor.release.add_permits(1);
    completed(&second_handle, &second).await;
    assert_eq!(second_executor.calls.load(Ordering::SeqCst), 1);
    shutdown(&second_handle).await;
}

#[tokio::test]
async fn closing_other_runtimes_does_not_stop_claimed_peer_background_jobs() {
    let (dir, mut store, peer, models) = fixture();
    let other = sibling(&mut store, &peer, &models);
    drop(store);
    let path = dir.0.join("lab.db");
    let executor = Arc::new(GatedExecutor::new(models));
    let claim = SessionClaim::acquire(&path, peer.session.clone()).unwrap();
    let owner = Runtime::open_claimed_with_options(
        &path,
        executor.clone(),
        RuntimeOptions::default(),
        claim,
    )
    .await
    .unwrap();
    let db = Connection::open(&path).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    let run = RunId::new();
    let operation = OperationId::new();
    let job = JobId::new();
    db.execute(
        "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
        params![run, peer.thread],
    )
    .unwrap();
    db.execute(
        "INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'bash','tool','{}','started')",
        params![operation, run, peer.thread],
    )
    .unwrap();
    db.execute(
        "INSERT INTO jobs(id,session,thread,operation,run,kind,state,title,background,epoch) VALUES(?1,?2,?3,?4,?5,'shell','running','fixture shell',1,0)",
        params![job, peer.session, peer.thread, operation, run],
    )
    .unwrap();
    let before = thread_snapshot(&db, &peer.thread);
    let observer = Runtime::open(&path, executor.clone()).await.unwrap();
    shutdown(&observer).await;
    assert_eq!(thread_snapshot(&db, &peer.thread), before);
    let other_owner = Runtime::open(&path, executor.clone()).await.unwrap();
    other_owner
        .activate_session(other.session, vec![])
        .await
        .unwrap();
    assert_eq!(thread_snapshot(&db, &peer.thread), before);
    shutdown(&other_owner).await;
    assert_eq!(thread_snapshot(&db, &peer.thread), before);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    shutdown(&owner).await;
}

#[tokio::test]
async fn recovering_crashed_session_does_not_recover_or_cancel_live_peer() {
    let (dir, mut store, crashed, models) = fixture();
    let live = sibling(&mut store, &crashed, &models);
    let (run, operation) = abandoned_model(&store, &crashed);
    drop(store);
    let path = dir.0.join("lab.db");
    let live_executor = Arc::new(GatedExecutor::new(models.clone()));
    let live_handle = Runtime::open(&path, live_executor.clone()).await.unwrap();
    live_handle
        .activate_session(live.session.clone(), vec![submit(&live, "live peer")])
        .await
        .unwrap();
    live_executor.started().await;
    let db = reader(&path);
    let before = thread_snapshot(&db, &live.thread);
    let crashed_executor = Arc::new(GatedExecutor::new(models));
    let recovered = Runtime::open_paused(&path, crashed_executor.clone())
        .await
        .unwrap();
    assert_eq!(thread_snapshot(&db, &live.thread), before);
    recovered
        .activate_session(crashed.session.clone(), vec![])
        .await
        .unwrap();
    assert_eq!(
        db.query_row("SELECT state FROM runs WHERE id=?1", [&run], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "interrupted"
    );
    assert_eq!(
        db.query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&operation],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "interrupted"
    );
    assert_eq!(thread_snapshot(&db, &live.thread), before);
    crashed_executor.assert_not_started().await;
    shutdown(&recovered).await;
    assert_eq!(thread_snapshot(&db, &live.thread), before);
    assert_eq!(live_executor.cancellations.load(Ordering::SeqCst), 0);
    live_executor.release.add_permits(1);
    completed(&live_handle, &live).await;
    shutdown(&live_handle).await;
}

#[test]
fn simultaneous_fresh_store_opens_initialize_one_complete_schema() {
    let dir = TempDir(
        std::env::temp_dir().join(format!("bob-kernel-concurrency-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir(&dir.0).unwrap();
    let path = dir.0.join("sessions.sqlite");
    let barrier = Arc::new(Barrier::new(8));
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = &path;
                let barrier = barrier.clone();
                scope.spawn(move || -> anyhow::Result<(i64, String)> {
                    barrier.wait();
                    let store = Store::open(path)?;
                    let schema = store
                        .db
                        .query_row("PRAGMA user_version", [], |row| row.get(0))?;
                    let integrity = store
                        .db
                        .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                    let owners: i64 =
                        store
                            .db
                            .query_row("SELECT COUNT(*) FROM session_owners", [], |row| {
                                row.get(0)
                            })?;
                    assert_eq!(owners, 0);
                    Ok((schema, integrity))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    for result in results {
        let (schema, integrity) = result.expect("concurrent schema initialization failed");
        assert_eq!(schema, crate::store::SCHEMA);
        assert_eq!(integrity, "ok");
    }
}

#[test]
fn canonical_database_paths_share_claims_across_dot_components() {
    let (dir, store, root, _) = fixture();
    drop(store);
    std::fs::create_dir(dir.0.join("child")).unwrap();
    let path = dir.0.join("lab.db");
    let alias = dir.0.join("child").join("..").join(".").join("lab.db");
    assert_eq!(
        canonical_database_path(&path).unwrap(),
        canonical_database_path(&alias).unwrap()
    );
    let claim = SessionClaim::acquire(&path, root.session.clone()).unwrap();
    assert!(SessionClaim::acquire(&alias, root.session.clone()).is_err());
    drop(claim);
    assert!(SessionClaim::acquire(&alias, root.session).is_ok());
    assert_eq!(
        canonical_database_path(&dir.0.join("child").join("..").join("new.sqlite")).unwrap(),
        std::fs::canonicalize(&dir.0).unwrap().join("new.sqlite")
    );
}

#[cfg(unix)]
#[test]
fn canonical_symlink_aliases_cannot_bypass_session_claims() {
    use std::os::unix::fs::symlink;

    let (dir, store, root, _) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let directory_alias = dir.0.join("directory-alias");
    let file_alias = dir.0.join("file-alias.sqlite");
    symlink(&dir.0, &directory_alias).unwrap();
    symlink(&path, &file_alias).unwrap();
    let expected = canonical_database_path(&path).unwrap();
    let claim = SessionClaim::acquire(&path, root.session.clone()).unwrap();
    for alias in [&directory_alias.join("lab.db"), &file_alias] {
        assert_eq!(canonical_database_path(alias).unwrap(), expected);
        assert!(SessionClaim::acquire(alias, root.session.clone()).is_err());
    }
    assert_eq!(
        canonical_database_path(&directory_alias.join("not-created.sqlite")).unwrap(),
        expected.parent().unwrap().join("not-created.sqlite")
    );
    drop(claim);
    assert!(SessionClaim::acquire(&file_alias, root.session).is_ok());
}

#[tokio::test]
async fn peer_commits_do_not_invalidate_owned_watch_but_global_sequences_remain_global() {
    let (dir, mut store, root, models) = fixture();
    let peer = sibling(&mut store, &root, &models);
    drop(store);
    let path = dir.0.join("lab.db");
    let owned = Runtime::open(&path, Arc::new(GatedExecutor::new(models.clone())))
        .await
        .unwrap();
    let other = Runtime::open(&path, Arc::new(GatedExecutor::new(models)))
        .await
        .unwrap();
    owned
        .activate_session(root.session.clone(), vec![])
        .await
        .unwrap();
    other
        .activate_session(peer.session.clone(), vec![])
        .await
        .unwrap();
    let mut watch = owned.subscribe();
    tokio::time::sleep(Duration::from_millis(75)).await;
    let initial = watch.borrow_and_update().sequence;
    let receipt = other
        .command(Command::SetPolicy {
            id: CommandId::new(),
            session: peer.session.clone(),
            policy: Policy::default(),
            expected_revision: 0,
        })
        .await
        .unwrap();
    assert!(receipt.sequence > initial);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), watch.changed())
            .await
            .is_err()
    );
    let view = owned
        .view(Some(root.thread.clone()), None, 100)
        .await
        .unwrap();
    assert_eq!(view.sequence, receipt.sequence);
    assert!(view
        .threads
        .iter()
        .all(|thread| thread.session == root.session));
    assert!(view
        .policies
        .iter()
        .all(|policy| policy.session == root.session));
    let receipt = owned
        .command(Command::SetPolicy {
            id: CommandId::new(),
            session: root.session.clone(),
            policy: Policy::default(),
            expected_revision: 0,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), watch.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(watch.borrow().sequence, receipt.sequence);
    shutdown(&other).await;
    shutdown(&owned).await;
}

#[tokio::test]
async fn writer_contention_preserves_completed_worker_until_one_successful_publication() {
    let (dir, store, root, models) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let executor = Arc::new(GatedExecutor::new(models));
    let handle = Runtime::open(&path, executor.clone()).await.unwrap();
    handle
        .activate_session(root.session.clone(), vec![submit(&root, "contention gate")])
        .await
        .unwrap();
    executor.started().await;
    let writer = Connection::open(&path).unwrap();
    writer.busy_timeout(Duration::from_secs(2)).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    executor.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), executor.returned.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let view = handle
            .view(Some(root.thread.clone()), None, 100)
            .await
            .unwrap();
        assert!(
            view.error.is_none(),
            "temporary writer contention halted the actor: {:?}",
            view.error
        );
        assert!(!view
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Assistant));
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.cancellations.load(Ordering::SeqCst), 0);
    writer.execute_batch("COMMIT").unwrap();
    completed(&handle, &root).await;
    shutdown(&handle).await;
    assert_eq!(
        writer
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE thread=?1 AND kind='assistant'",
                [&root.thread],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        writer
            .query_row(
                "SELECT COUNT(*) FROM usage WHERE thread=?1",
                [&root.thread],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn generation_change_fences_late_model_completion_and_new_commands() {
    let (dir, store, root, models) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let mut gated = GatedExecutor::new(models);
    gated.ignore_cancel = true;
    let executor = Arc::new(gated);
    let handle = Runtime::open(&path, executor.clone()).await.unwrap();
    handle
        .activate_session(root.session.clone(), vec![submit(&root, "old generation")])
        .await
        .unwrap();
    executor.started().await;
    let db = Connection::open(&path).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE session_owners SET generation=?1 WHERE session=?2",
            params![uuid::Uuid::new_v4().to_string(), root.session],
        )
        .unwrap(),
        1
    );
    executor.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), executor.returned.acquire())
        .await
        .expect("late completion was not produced")
        .unwrap()
        .forget();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .view(Some(root.thread.clone()), None, 100)
                .await
                .unwrap();
            if view.error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("superseded owner did not report its generation fence");
    assert!(handle
        .command(submit(&root, "stale owner command"))
        .await
        .is_err());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
        .await
        .expect("fenced runtime did not shut down");
    for sql in [
        "SELECT COUNT(*) FROM entries WHERE thread=?1 AND kind='assistant'",
        "SELECT COUNT(*) FROM usage WHERE thread=?1",
        "SELECT COUNT(*) FROM submissions WHERE thread=?1 AND text='stale owner command'",
    ] {
        assert_eq!(
            db.query_row(sql, [&root.thread], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
