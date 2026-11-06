use super::*;
use crate::test_support::TestDir;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn fake(token: &str) -> Credential {
    Credential {
        token: token.into(),
        refresh_token: Some("fake-refresh".into()),
        extra: Default::default(),
    }
}

struct Worker(Option<Child>);

impl Worker {
    fn start(root: &Path, mode: &str, key: &str) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "auth::storage::tests::auth_subprocess_worker",
                "--nocapture",
            ])
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("BOB_AUTH_TEST_ROOT", root)
            .env("BOB_AUTH_TEST_MODE", mode)
            .env("BOB_AUTH_TEST_KEY", key)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(Some(child))
    }

    fn finish(mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "auth subprocess timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing subprocess signal {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn auth_subprocess_worker() {
    let Ok(mode) = std::env::var("BOB_AUTH_TEST_MODE") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("BOB_AUTH_TEST_ROOT").unwrap());
    let key = std::env::var("BOB_AUTH_TEST_KEY").unwrap();
    let path = root.join(".bob/auth.json");
    assert_eq!(super::super::auth_path(), path);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        match mode.as_str() {
            "set" => {
                for _ in 0..24 {
                    AuthStore::set_credential(&key, fake("login"))
                        .await
                        .unwrap();
                }
            }
            "remove" => {
                for _ in 0..24 {
                    AuthStore::remove_credential(&key).await.unwrap();
                }
            }
            "refresh" => {
                std::fs::write(
                    root.join(format!("started-{}", std::process::id())),
                    "ready",
                )
                .unwrap();
                let result = access_token_with(&path, &key, &[], "missing", |_| async {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(root.join("network-count"))
                        .unwrap()
                        .write_all(b"request\n")
                        .unwrap();
                    std::fs::write(root.join("entered"), "ready").unwrap();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !root.join("release").exists() {
                        assert!(Instant::now() < deadline, "refresh gate timed out");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(json!({"access_token": "stale-refresh", "expires_in": 3600}))
                })
                .await;
                std::fs::write(
                    root.join("result"),
                    match result {
                        Ok(token) => token,
                        Err(_) => "removed".into(),
                    },
                )
                .unwrap();
            }
            _ => panic!("unknown auth fixture mode"),
        }
    });
}

#[test]
fn subprocess_refreshes_coalesce_after_locked_reread() {
    let root = TestDir::new();
    root.write(
        ".bob/auth.json",
        r#"{"providers":{"one":{"token":"expired","refresh_token":"fake"}}}"#,
    );
    let first = Worker::start(&root, "refresh", "one");
    wait_for(&root.join("entered"));
    let second = Worker::start(&root, "refresh", "one");
    wait_for(&root.join(format!("started-{}", second.0.as_ref().unwrap().id())));
    root.write("release", "go");
    first.finish();
    second.finish();
    assert_eq!(
        std::fs::read_to_string(root.join("network-count")).unwrap(),
        "request\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("result")).unwrap(),
        "stale-refresh"
    );
}

#[tokio::test]
async fn missing_logout_and_failed_token_exchange_do_not_write_credentials() {
    let root = TestDir::new();
    let path = root.join("auth.json");
    assert!(!update_at(&path, "one", |_| Ok(None)).await.unwrap());
    assert!(!path.exists());
    assert!(
        store_tokens_at(&path, "one", &json!({"error":"denied"}), &[])
            .await
            .is_err()
    );
    assert!(!path.exists());
    update_at(&path, "one", |_| Ok(Some(fake("expired"))))
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(access_token_with(&path, "one", &[], "missing", |_| async {
        anyhow::bail!("fake provider rejected refresh")
    })
    .await
    .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn subprocess_auth_separate_key_writes_and_removals_preserve_peers() {
    let root = TestDir::new();
    let path = root.write(".bob/auth.json", r#"{"future":{"keep":true},"providers":{"remove":{"token":"old"},"peer":{"token":"keep","future":"retained"}}}"#);
    let writers: Vec<_> = ["anthropic", "openai", "mcp:server"]
        .into_iter()
        .map(|key| Worker::start(&root, "set", key))
        .collect();
    let remover = Worker::start(&root, "remove", "remove");
    for writer in writers {
        writer.finish();
    }
    remover.finish();
    let document = read_document(&path).unwrap();
    for key in ["anthropic", "openai", "mcp:server"] {
        assert_eq!(document["providers"][key]["token"], "login");
    }
    assert!(document["providers"].get("remove").is_none());
    assert_eq!(document["providers"]["peer"]["future"], "retained");
    assert_eq!(document["future"]["keep"], true);
}

#[test]
fn subprocess_stale_refresh_cannot_overwrite_login_or_resurrect_logout() {
    for mutation in ["set", "remove"] {
        let root = TestDir::new();
        let path = root.write(
            ".bob/auth.json",
            r#"{"providers":{"anthropic":{"token":"expired","refresh_token":"fake"}}}"#,
        );
        let refresh = Worker::start(&root, "refresh", "anthropic");
        wait_for(&root.join("entered"));
        Worker::start(&root, mutation, "anthropic").finish();
        root.write("release", "go");
        refresh.finish();
        let store = load_at(&path).unwrap();
        if mutation == "set" {
            assert_eq!(store.get("anthropic").unwrap().token, "login");
            assert_eq!(
                std::fs::read_to_string(root.join("result")).unwrap(),
                "login"
            );
        } else {
            assert!(store.get("anthropic").is_none());
            assert_eq!(
                std::fs::read_to_string(root.join("result")).unwrap(),
                "removed"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_coalesces_same_key_without_blocking_other_keys() {
    let root = TestDir::new();
    let path = root.join("auth.json");
    update_at(&path, "one", |_| Ok(Some(fake("expired"))))
        .await
        .unwrap();
    update_at(&path, "two", |_| Ok(Some(fake("expired"))))
        .await
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let first_path = path.clone();
    let first = tokio::spawn(async move {
        access_token_with(&first_path, "one", &[], "missing", |_| async {
            entered_tx.send(()).unwrap();
            release_rx.await.unwrap();
            Ok(json!({"access_token":"first", "expires_in":3600}))
        })
        .await
        .unwrap()
    });
    entered_rx.await.unwrap();
    let second_path = path.clone();
    let second = tokio::spawn(async move {
        access_token_with(&second_path, "one", &[], "missing", |_| async {
            panic!("duplicate refresh")
        })
        .await
        .unwrap()
    });
    let other = tokio::time::timeout(
        Duration::from_secs(1),
        access_token_with(&path, "two", &[], "missing", |_| async {
            Ok(json!({"access_token":"other", "expires_in":3600}))
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(other, "other");
    release_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap(), "first");
    assert_eq!(second.await.unwrap(), "first");
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_cancellation_releases_key_and_does_not_publish() {
    let root = TestDir::new();
    let path = root.join("auth.json");
    update_at(&path, "one", |_| Ok(Some(fake("expired"))))
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let pending = access_token_with(&path, "one", &[], "missing", |_| async {
        std::future::pending::<anyhow::Result<Value>>().await
    });
    assert!(tokio::time::timeout(Duration::from_millis(30), pending)
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let token = access_token_with(&path, "one", &[], "missing", |_| async {
        Ok(json!({"access_token":"replacement"}))
    })
    .await
    .unwrap();
    assert_eq!(token, "replacement");
}

#[tokio::test]
async fn refresh_revision_detects_identical_login_aba() {
    let root = TestDir::new();
    let path = root.join("auth.json");
    update_at(&path, "one", |_| Ok(Some(fake("same"))))
        .await
        .unwrap();
    let token = access_token_with(&path, "one", &[], "missing", |_| async {
        update_at(&path, "one", |_| Ok(None)).await.unwrap();
        update_at(&path, "one", |_| Ok(Some(fake("same"))))
            .await
            .unwrap();
        Ok(json!({"access_token":"stale"}))
    })
    .await
    .unwrap();
    assert_eq!(token, "same");
    assert_eq!(load_at(&path).unwrap().get("one").unwrap().token, "same");
}

#[tokio::test]
async fn strict_mutations_preserve_malformed_unreadable_and_unknown_data() {
    let root = TestDir::new();
    for raw in [
        "{broken",
        r#"{"providers":{"one":{"token":"secret","refresh_token":17}}}"#,
        r#"{"providers":null}"#,
        r#"{"_revisions":[]}"#,
    ] {
        let path = root.write("auth.json", raw);
        let error = update_at(&path, "one", |_| Ok(Some(fake("new"))))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("secret"));
        assert!(update_at(&path, "one", |_| Ok(None)).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert!(!path.with_extension("json.corrupt").exists());
    }
    let directory = root.join("directory.json");
    std::fs::create_dir(&directory).unwrap();
    assert!(update_at(&directory, "one", |_| Ok(None)).await.is_err());
    assert!(directory.is_dir());
}
