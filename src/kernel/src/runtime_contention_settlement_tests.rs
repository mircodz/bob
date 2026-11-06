use super::tests::{start_run, NoDispatch};
use super::*;
use crate::store::tests::fixture;

#[tokio::test]
async fn claimed_startup_retries_busy_adoption_without_releasing_claim() {
    let (dir, store, root, models) = fixture();
    let path = dir.0.join("lab.db");
    drop(store);
    let claim = crate::SessionClaim::acquire(&path, root.session.clone()).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let opening = Runtime::open_claimed_with_options(
        &path,
        Arc::new(NoDispatch(models)),
        RuntimeOptions::default(),
        claim,
    );
    tokio::pin!(opening);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut opening)
            .await
            .is_err()
    );
    assert!(crate::SessionClaim::acquire_initialized(&path, root.session.clone()).is_err());
    blocker.execute_batch("ROLLBACK").unwrap();
    let handle = tokio::time::timeout(Duration::from_secs(3), opening)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.session(root.session).await.unwrap().thread,
        root.thread
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_retains_worker_and_owner_past_reply_deadline_until_unknown_commits() {
    let (dir, store, root, models) = fixture();
    let path = dir.0.join("lab.db");
    let mut actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
    actor.paused = true;
    actor.store.db.busy_timeout(Duration::ZERO).unwrap();
    let run = start_run(&actor, &root.thread);
    let operation = OperationId::new();
    actor.store.db.execute(
        "INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'shell','tool','effect','{}','started')",
        params![operation, run, root.thread],
    ).unwrap();
    entry(
        &actor.store.db,
        &root.thread,
        "user",
        "reader remains available",
        EntryData::default(),
    )
    .unwrap();
    let sequence: i64 = actor
        .store
        .db
        .query_row("SELECT MAX(seq) FROM entries", [], |r| r.get(0))
        .unwrap();
    let (done, _completed) = mpsc::channel(1);
    let (signalled, observed) = oneshot::channel();
    actor.spawn(operation.clone(), done, |mut cancel| async move {
        cancelled(&mut cancel).await;
        let _ = signalled.send(());
        std::future::pending::<anyhow::Result<Output>>().await
    });
    let worker = actor.workers[&operation].task.abort_handle();
    let (tx, rx) = mpsc::channel(128);
    let handle = Handle {
        tx,
        live: actor.live.subscribe(),
        reader: crate::read_worker::Reader::open(&path).await.unwrap(),
        thread: Arc::new(Mutex::new(Some(std::thread::spawn(|| Ok(()))))),
    };
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let checks = async {
        let shutdown = handle.shutdown();
        tokio::pin!(shutdown);
        tokio::select! {
            result = &mut shutdown => panic!("shutdown finished before cancellation was observed: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(1), observed) => result.unwrap().unwrap(),
        }
        let error = tokio::time::timeout(Duration::from_secs(6), shutdown)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("still settling"));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !worker.is_finished(),
            "worker aborted before unknown publication"
        );
        assert!(
            handle.thread.lock().unwrap().is_some(),
            "shutdown joined on error"
        );
        assert!(crate::SessionClaim::acquire_initialized(&path, root.session.clone()).is_err());
        assert_eq!(
            blocker
                .query_row(
                    "SELECT state FROM operations WHERE id=?1",
                    [&operation],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "started"
        );
        assert!(handle
            .entry_text(root.thread.clone(), sequence, 0, 100)
            .await
            .is_ok());
        assert!(handle
            .shutdown()
            .await
            .unwrap_err()
            .to_string()
            .contains("still settling"));
        assert!(handle
            .command(Command::Resume {
                id: CommandId::new(),
                thread: root.thread.clone()
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("closing"));
        blocker.execute_batch("ROLLBACK").unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle.tx.closed())
            .await
            .unwrap();
        assert_eq!(
            blocker
                .query_row(
                    "SELECT state FROM operations WHERE id=?1",
                    [&operation],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "unknown"
        );
        handle.shutdown().await.unwrap();
        assert!(handle.thread.lock().unwrap().is_none());
        assert!(crate::SessionClaim::acquire_initialized(&path, root.session.clone()).is_ok());
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(actor.run(rx), checks)
    })
    .await
    .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn shutdown_joins_a_closed_reply_and_propagates_actor_failure() {
    for fail in [false, true] {
        let (dir, store, _, models) = fixture();
        let actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
        let live = actor.live.subscribe();
        drop(actor);
        let (tx, mut rx) = mpsc::channel(1);
        let thread = std::thread::spawn(move || {
            let Some(Input::Shutdown(reply)) = rx.blocking_recv() else {
                panic!("expected a queued shutdown request");
            };
            drop(reply);
            if fail {
                anyhow::bail!("fixture actor failure");
            }
            Ok(())
        });
        let handle = Handle {
            tx,
            live,
            reader: crate::read_worker::Reader::open(&dir.0.join("lab.db"))
                .await
                .unwrap(),
            thread: Arc::new(Mutex::new(Some(thread))),
        };
        let result = tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .unwrap();
        if fail {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("fixture actor failure"));
        } else {
            result.unwrap();
        }
        assert!(handle.thread.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn closed_command_channel_keeps_retrying_without_starving_settlement() {
    let (dir, store, root, models) = fixture();
    let path = dir.0.join("lab.db");
    let mut actor = Actor::new_owned(store, Arc::new(NoDispatch(models))).unwrap();
    actor.paused = true;
    actor.store.db.busy_timeout(Duration::ZERO).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    let release = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(crate::SessionClaim::acquire_initialized(&path, root.session.clone()).is_err());
        blocker.execute_batch("ROLLBACK").unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(actor.run(rx), release)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(crate::SessionClaim::acquire_initialized(&path, root.session).is_ok());
}
