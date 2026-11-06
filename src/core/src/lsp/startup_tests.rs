use super::*;
use crate::test_support::TestDir;
use std::time::Duration;

fn fixture_config(dir: &TestDir) -> LspServerConfig {
    dir.write(".lsp-startup-fixture", "enabled");
    LspServerConfig {
        name: "startup-fixture".into(),
        command: std::env::current_exe()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string(),
        args: vec![
            "--exact".into(),
            "lsp::startup_tests::stalled_server_fixture".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        extensions: vec!["rs".into()],
        root: ".".into(),
    }
}

#[test]
#[ignore = "subprocess fixture for LSP startup ownership tests"]
fn stalled_server_fixture() {
    use std::io::{BufRead, Read};

    if std::fs::read_to_string(".lsp-startup-fixture")
        .ok()
        .as_deref()
        != Some("enabled")
    {
        return;
    }
    std::fs::write("spawned", std::process::id().to_string()).unwrap();
    let mut input = std::io::stdin().lock();
    let mut content_length = None;
    loop {
        let mut line = String::new();
        assert_ne!(input.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.strip_prefix("Content-Length:") {
            content_length = Some(length.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    input.read_exact(&mut body).unwrap();
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["method"], "initialize");
    std::fs::write("handshake", "initialize").unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

async fn wait_for_startup_exit(tasks: &[tokio::task::AbortHandle]) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while tasks.iter().any(|task| !task.is_finished()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("manager drop must cancel startup rather than wait for its timeout");
}

fn startup_handles(manager: &LspManager) -> Vec<tokio::task::AbortHandle> {
    manager
        .startup
        .lock()
        .unwrap()
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn drop_before_startup_is_scheduled_does_not_spawn_process() {
    let dir = TestDir::new();
    let manager = LspManager::start(&[fixture_config(&dir)], dir.path());
    let weak = Arc::downgrade(&manager);
    let tasks = startup_handles(&manager);
    assert_eq!(tasks.len(), 1);
    assert!(!dir.join("spawned").exists());

    drop(manager);
    assert!(weak.upgrade().is_none());
    wait_for_startup_exit(&tasks).await;
    assert!(!dir.join("spawned").exists());
    assert!(!dir.join("handshake").exists());
}

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "current_thread")]
async fn drop_during_stalled_handshake_terminates_process() {
    let dir = TestDir::new();
    let manager = LspManager::start(&[fixture_config(&dir)], dir.path());
    let weak = Arc::downgrade(&manager);
    let tasks = startup_handles(&manager);
    tokio::time::timeout(Duration::from_secs(10), async {
        while std::fs::read_to_string(dir.join("handshake"))
            .ok()
            .as_deref()
            != Some("initialize")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture must receive initialize before the manager is dropped");
    let pid = std::fs::read_to_string(dir.join("spawned"))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let process = ProcessProbe::new(pid);
    assert!(process.is_running());
    assert_eq!(manager.statuses()[0].1, Health::Starting);
    assert!(manager.client_by_name("startup-fixture").is_none());

    drop(manager);
    assert!(weak.upgrade().is_none());
    wait_for_startup_exit(&tasks).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while process.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled handshake must terminate and reap its language server");
}

#[cfg(windows)]
#[tokio::test]
async fn native_windows_lsp_process_preserves_unicode_paths_and_cleans_up() {
    use crate::test_support::windows_rpc::{Process, Server};

    let dir = TestDir::new();
    let server = Server::new(&dir, true);
    let config = LspServerConfig {
        name: "native-windows-fixture".into(),
        command: server.command,
        args: server.args,
        extensions: vec!["rs".into()],
        root: ".".into(),
    };
    let client = tokio::time::timeout(
        Duration::from_secs(10),
        LspClient::start(&config, &server.root),
    )
    .await
    .unwrap()
    .unwrap();
    let params = json!({"text":"quotes \" and 日本語 😀\r\nnext"});
    let reply = client
        .request("fixture/echo", params.clone())
        .await
        .unwrap();
    let process = Process::open(reply["pid"].as_u64().unwrap().try_into().unwrap());
    assert_eq!(reply["echo"], params);
    assert_eq!(reply["text"], "日本語 😀\nnext");
    assert_eq!(
        std::fs::canonicalize(reply["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&server.root).unwrap()
    );
    assert!(client
        .request("fixture/error", Value::Null)
        .await
        .unwrap_err()
        .to_string()
        .contains("fixture rejection"));
    assert_eq!(
        client
            .request("fixture/echo", json!({"after":"error"}))
            .await
            .unwrap()["echo"],
        json!({"after":"error"})
    );
    let mut pending = Box::pin(client.request("fixture/stall", Value::Null));
    assert!(futures::poll!(&mut pending).is_pending());
    drop(pending);
    drop(client);
    process.wait_exit().await;
}

#[cfg(unix)]
struct ProcessProbe(i32);

#[cfg(unix)]
impl ProcessProbe {
    fn new(pid: u32) -> Self {
        Self(pid.try_into().unwrap())
    }

    fn is_running(&self) -> bool {
        if unsafe { libc::kill(self.0, 0) } == 0 {
            return true;
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        false
    }
}

#[cfg(windows)]
struct ProcessProbe(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl ProcessProbe {
    fn new(pid: u32) -> Self {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(!handle.is_null(), "{}", std::io::Error::last_os_error());
        Self(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    fn is_running(&self) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;

        let mut code = 0;
        assert_ne!(
            unsafe { GetExitCodeProcess(self.0.as_raw_handle(), &mut code) },
            0
        );
        code == 259
    }
}
