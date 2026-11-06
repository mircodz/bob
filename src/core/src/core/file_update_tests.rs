use super::*;
use crate::test_support::TestDir;

#[test]
fn file_lock_subprocess_worker() {
    let Some(root) = std::env::var_os("BOB_FILE_UPDATE_LOCK_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let _guard = FileLock::acquire(&root.join("stable.lock")).unwrap();
    std::fs::write(root.join("locked"), "ready").unwrap();
    loop {
        std::thread::park_timeout(Duration::from_secs(30));
    }
}

#[test]
fn subprocess_lock_authority_survives_pid_contents_and_releases_on_exit() {
    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = TestDir::new();
    let lock = root.write(
        "stable.lock",
        "999999999 stale-looking PID is not authority",
    );
    let mut process = Process(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "core::file_update::tests::file_lock_subprocess_worker",
                "--nocapture",
            ])
            .env("HOME", &*root)
            .env("USERPROFILE", &*root)
            .env("BOB_FILE_UPDATE_LOCK_ROOT", &*root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("locked").exists() {
        assert!(Instant::now() < deadline, "lock worker did not acquire");
        std::thread::sleep(Duration::from_millis(10));
    }
    let contender = FileLock::open(&lock).unwrap();
    assert!(!FileLock::try_acquire(&contender).unwrap());
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    assert!(FileLock::try_acquire(&contender).unwrap());
    assert_eq!(
        std::fs::read_to_string(lock).unwrap(),
        "999999999 stale-looking PID is not authority"
    );
}

#[test]
fn replacement_preserves_old_fixed_temp_and_cleans_unique_temps() {
    let root = TestDir::new();
    let path = root.write("auth.json", "old");
    let legacy_temp = root.write("auth.json.tmp", "unrelated");
    for value in [b"first".as_slice(), b"second".as_slice()] {
        atomic_replace(&path, value, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), value);
        assert_eq!(std::fs::read(&legacy_temp).unwrap(), b"unrelated");
        assert_eq!(std::fs::read_dir(&*root).unwrap().count(), 2);
    }
}

#[test]
fn failed_replacement_preserves_target_and_removes_only_owned_temp() {
    let root = TestDir::new();
    let path = root.join("destination");
    std::fs::create_dir(&path).unwrap();
    let sentinel = root.write("destination/keep", "sentinel");
    assert!(atomic_replace(&path, b"replacement", true).is_err());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"sentinel");
    assert_eq!(std::fs::read_dir(&*root).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn private_replacement_is_owner_only_and_config_keeps_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let root = TestDir::new();
    let path = root.write("auth.json", "old");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    atomic_replace(&path, b"config", false).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    atomic_replace(&path, b"secret", true).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test(flavor = "current_thread")]
async fn async_lock_wait_yields_and_cancellation_does_not_acquire_later() {
    let root = TestDir::new();
    let path = root.join("stable.lock");
    let guard = FileLock::acquire(&path).unwrap();
    let wait = FileLock::acquire_async(&path);
    assert!(tokio::time::timeout(Duration::from_millis(40), wait)
        .await
        .is_err());
    drop(guard);
    let guard = FileLock::acquire_async(&path).await.unwrap();
    assert!(path.exists());
    drop(guard);
    assert!(path.exists());
}

#[cfg(unix)]
#[test]
fn aliases_share_update_identity_and_dangling_links_are_not_overwritten() {
    use std::os::unix::fs::symlink;
    let root = TestDir::new();
    let path = root.write("config.toml", "# keep\nprovider = 'anthropic'\n");
    let alias = root.join("alias.toml");
    symlink(&path, &alias).unwrap();
    assert_eq!(update_path(&path).unwrap(), update_path(&alias).unwrap());
    crate::core::config::set_default_model_at(&alias, "openai", "fixture").unwrap();
    assert!(std::fs::symlink_metadata(&alias)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_to_string(&path).unwrap().contains("fixture"));
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .starts_with("# keep"));
    let parent = root.join("directory");
    std::fs::create_dir(&parent).unwrap();
    let parent_alias = root.join("directory-alias");
    symlink(&parent, &parent_alias).unwrap();
    let real_file = parent.join("config.toml");
    let alias_file = parent_alias.join("config.toml");
    assert_eq!(
        update_path(&real_file).unwrap(),
        update_path(&alias_file).unwrap()
    );
    crate::core::config::set_default_model_at(&alias_file, "copilot", "fixture").unwrap();
    let identity = update_path(&real_file).unwrap();
    assert_eq!(identity, update_path(&alias_file).unwrap());
    let _guard = FileLock::acquire(&sibling(&identity, ".lock")).unwrap();
    let contender = FileLock::open(&sibling(&update_path(&alias_file).unwrap(), ".lock")).unwrap();
    assert!(!FileLock::try_acquire(&contender).unwrap());
    let dangling = root.join("dangling.toml");
    symlink(root.join("missing.toml"), &dangling).unwrap();
    assert!(update_path(&dangling).is_err());
    assert!(crate::core::config::set_default_model_at(&dangling, "openai", "fixture").is_err());
    assert!(!root.join("missing.toml").exists());
    assert!(std::fs::symlink_metadata(dangling)
        .unwrap()
        .file_type()
        .is_symlink());
}
