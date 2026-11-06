use crate::store::tests::fixture;
use crate::store::Store;
use crate::*;
use async_trait::async_trait;
use bob_types::Completion;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct ProcessGate(ModelInfo);
#[async_trait]
impl Executor for ProcessGate {
    fn models(&self) -> Vec<ModelInfo> {
        vec![self.0.clone()]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        _: RequestSpec,
        _: watch::Receiver<bool>,
        _: ModelProgress,
    ) -> anyhow::Result<Completion> {
        println!("BOB_KERNEL_PROCESS_MODEL_STARTED");
        std::io::stdout().flush().unwrap();
        std::future::pending().await
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
async fn process_claim_holder() {
    let Some(path) = std::env::var_os("BOB_KERNEL_PROCESS_TEST_DB") else {
        return;
    };
    let session = SessionId(std::env::var("BOB_KERNEL_PROCESS_TEST_SESSION").unwrap());
    let catalog = SessionCatalog::open(Path::new(&path)).unwrap();
    let snapshot = catalog.session(session.clone()).unwrap();
    drop(catalog);
    let executor = Arc::new(ProcessGate(ModelInfo {
        id: snapshot.profile.model,
        context_window: 128_000,
    }));
    let claim = SessionClaim::acquire(Path::new(&path), session.clone()).unwrap();
    let handle = Runtime::open_claimed_with_options(
        Path::new(&path),
        executor,
        RuntimeOptions::default(),
        claim,
    )
    .await
    .unwrap();
    handle
        .activate_session(
            session,
            vec![Command::Submit {
                id: CommandId::new(),
                thread: snapshot.thread,
                text: "process model gate".into(),
            }],
        )
        .await
        .unwrap();
    std::future::pending::<()>().await;
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_child(path: &Path, session: &SessionId) -> ChildGuard {
    let mut child = ChildGuard(
        ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "concurrency_process_tests::process_claim_holder",
                "--nocapture",
            ])
            .env("BOB_KERNEL_PROCESS_TEST_DB", path)
            .env("BOB_KERNEL_PROCESS_TEST_SESSION", &session.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.contains("BOB_KERNEL_PROCESS_MODEL_STARTED") => {
                    let _ = tx.send(());
                    return;
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(15))
        .expect("separate process did not reach model gate");
    child
}

#[test]
fn separate_processes_overlap_and_crash_releases_only_the_dead_session_claim() {
    let (dir, mut store, first, models) = fixture();
    let snapshot = crate::sessions::session(&store.db, &first.session).unwrap();
    let second = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: snapshot.workspace,
                profile: snapshot.profile,
                policy: Policy::default(),
            },
            &models,
        )
        .unwrap();
    let path = dir.0.join("lab.db");
    drop(store);
    let mut first_process = start_child(&path, &first.session);
    let second_process = start_child(&path, &second.session);
    assert!(SessionClaim::acquire(&path, first.session.clone()).is_err());
    assert!(SessionClaim::acquire(&path, second.session.clone()).is_err());
    let store = Store::open(&path).unwrap();
    let live: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE name='model' AND state='started'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 2);
    drop(store);
    first_process.0.kill().unwrap();
    first_process.0.wait().unwrap();
    let recovered = SessionClaim::acquire(&path, first.session.clone()).unwrap();
    assert!(SessionClaim::acquire(&path, second.session.clone()).is_err());
    let mut store = Store::open(&path).unwrap();
    store.adopt_claim(recovered, true).unwrap();
    let peer: String = store.db.query_row("SELECT o.state FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND o.name='model'", [&second.session], |r| r.get(0)).unwrap();
    assert_eq!(peer, "started");
    let recovered: String = store.db.query_row("SELECT o.state FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND o.name='model'", [&first.session], |r| r.get(0)).unwrap();
    assert_eq!(recovered, "interrupted");
    drop(store);
    drop(second_process);
    assert!(SessionClaim::acquire(&path, second.session).is_ok());
}
