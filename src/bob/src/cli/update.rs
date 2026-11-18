use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RELEASE: &str = "https://api.github.com/repos/mircodz/bob/releases/tags/edge";
/// Installed in this order, so a busy helper stops the update before bob changes.
const EXECUTABLES: [&str; 2] = ["bob-exec", "bob"];
const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some("x86_64-pc-windows-msvc")
} else {
    None
};

struct Swap {
    target: PathBuf,
    staged: PathBuf,
    aside: PathBuf,
}

impl Swap {
    fn new(directory: &Path, name: &str) -> Self {
        let pid = std::process::id();
        let file =
            |tag: &str| directory.join(format!("{name}{tag}{}", std::env::consts::EXE_SUFFIX));
        Self {
            target: file(""),
            staged: file(&format!(".{pid}.new")),
            aside: file(&format!(".{pid}.old")),
        }
    }
}

struct Download {
    swap: Swap,
    url: String,
    sha256: String,
}

pub async fn run() -> Result<()> {
    let target = TARGET.context(
        "there is no prebuilt bob for this platform; build it from source with cargo install",
    )?;
    let executable = std::env::current_exe().context("locate the running bob")?;
    let directory = executable
        .parent()
        .context("the running bob has no parent directory")?;
    remove_leftovers(directory);
    let client = reqwest::Client::builder()
        .user_agent(bob_core::USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()?;
    let response = client
        .get(RELEASE)
        .send()
        .await
        .context("fetch the edge release from GitHub")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("no build of main has been published yet; try again once CI on main has finished");
    }
    let release: serde_json::Value = response.error_for_status()?.json().await?;
    let commit = release["target_commitish"].as_str().unwrap_or("unknown");
    let build = format!("main @ {}", commit.get(..7).unwrap_or(commit));
    let url = |name: &str| {
        release["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|asset| asset["name"] == name)
            .and_then(|asset| asset["browser_download_url"].as_str())
            .map(str::to_owned)
            .with_context(|| {
                format!(
                    "the edge release has no {name}; CI may still be publishing it, so try again in a minute"
                )
            })
    };
    let sums = client
        .get(url("SHA256SUMS")?)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let mut downloads = Vec::new();
    for name in EXECUTABLES {
        let asset = format!("{name}-{target}{}", std::env::consts::EXE_SUFFIX);
        let expected = checksum(&sums, &asset)
            .with_context(|| format!("SHA256SUMS does not list {asset}"))?
            .to_owned();
        downloads.push(Download {
            swap: Swap::new(directory, name),
            url: url(&asset)?,
            sha256: expected,
        });
    }
    if downloads.iter().all(|download| {
        std::fs::read(&download.swap.target).is_ok_and(|bytes| sha256(&bytes) == download.sha256)
    }) {
        println!("bob is already up to date ({build}).");
        return Ok(());
    }
    println!("Downloading {build} for {target} …");
    let result = download_and_install(&client, &downloads).await;
    for download in &downloads {
        let _ = std::fs::remove_file(&download.swap.staged);
        if result.is_ok() {
            let _ = std::fs::remove_file(&download.swap.aside);
        }
    }
    let version = result?;
    println!("Installed {version} in {}.", directory.display());
    println!("Restart any open bob sessions to use it.");
    Ok(())
}

async fn download_and_install(client: &reqwest::Client, downloads: &[Download]) -> Result<String> {
    for download in downloads {
        let bytes = client
            .get(&download.url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        ensure!(
            sha256(&bytes) == download.sha256,
            "{} does not match its published checksum; the release may have been replaced mid-download, so try again",
            download.url
        );
        write_executable(&download.swap.staged, &bytes)?;
    }
    let bob = &downloads.last().context("nothing to install")?.swap;
    let version = version_of(&bob.staged)
        .await
        .context("the downloaded bob does not run on this machine")?;
    install(
        &downloads
            .iter()
            .map(|download| &download.swap)
            .collect::<Vec<_>>(),
    )?;
    Ok(version)
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

async fn version_of(path: &Path) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(path)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("it did not answer --version within 30 seconds")??;
    ensure!(
        output.status.success(),
        "--version exited with {}",
        output.status
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Moves each target aside and its staged replacement into place, restoring
/// every earlier pair if a later one fails.
fn install(swaps: &[&Swap]) -> Result<()> {
    for (index, swap) in swaps.iter().enumerate() {
        if let Err(error) = replace(swap) {
            for done in swaps[..index].iter().rev() {
                let _ = std::fs::rename(&done.target, &done.staged);
                let _ = std::fs::rename(&done.aside, &done.target);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn replace(swap: &Swap) -> Result<()> {
    let moved = match std::fs::rename(&swap.target, &swap.aside) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            let hint = if cfg!(windows) {
                ". If an open bob session is running a shell command, wait for it to finish and run bob update again"
            } else {
                ""
            };
            return Err(error)
                .with_context(|| format!("could not move {} aside{hint}", swap.target.display()));
        }
    };
    if let Err(error) = std::fs::rename(&swap.staged, &swap.target) {
        if moved {
            let _ = std::fs::rename(&swap.aside, &swap.target);
        }
        return Err(error).with_context(|| format!("could not install {}", swap.target.display()));
    }
    Ok(())
}

fn remove_leftovers(directory: &Path) {
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        if is_leftover(&entry.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn is_leftover(file: &str) -> bool {
    let Some(file) = file.strip_suffix(std::env::consts::EXE_SUFFIX) else {
        return false;
    };
    EXECUTABLES.iter().any(|name| {
        file.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| {
                rest.strip_suffix(".new")
                    .or_else(|| rest.strip_suffix(".old"))
            })
            .is_some_and(|pid| pid.parse::<u32>().is_ok())
    })
}

fn checksum<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (digest, name) = line.split_once(char::is_whitespace)?;
        (name.trim_start().trim_start_matches('*') == asset).then_some(digest)
    })
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("bob-update-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pair(directory: &Path) -> (Swap, Swap) {
        let helper = Swap::new(directory, "bob-exec");
        let bob = Swap::new(directory, "bob");
        std::fs::write(&helper.target, "old exec").unwrap();
        std::fs::write(&helper.staged, "new exec").unwrap();
        std::fs::write(&bob.target, "old bob").unwrap();
        std::fs::write(&bob.staged, "new bob").unwrap();
        (helper, bob)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn checksums_are_found_by_asset_name() {
        let sums = "aaa  bob-aarch64-apple-darwin\nbbb *bob-exec-aarch64-apple-darwin\n";
        assert_eq!(checksum(sums, "bob-aarch64-apple-darwin"), Some("aaa"));
        assert_eq!(checksum(sums, "bob-exec-aarch64-apple-darwin"), Some("bbb"));
        assert_eq!(checksum(sums, "bob"), None);
    }

    #[test]
    fn only_files_left_by_an_update_count_as_leftovers() {
        let suffix = std::env::consts::EXE_SUFFIX;
        assert!(is_leftover(&format!("bob.123.old{suffix}")));
        assert!(is_leftover(&format!("bob-exec.9.new{suffix}")));
        assert!(!is_leftover(&format!("bob{suffix}")));
        assert!(!is_leftover(&format!("bob-exec{suffix}")));
        assert!(!is_leftover(&format!("bob.old{suffix}")));
        assert!(!is_leftover(&format!("bobcat.1.old{suffix}")));
    }

    #[test]
    fn install_replaces_both_executables() {
        let directory = TestDir::new("install");
        let (helper, bob) = pair(&directory.0);
        install(&[&helper, &bob]).unwrap();
        assert_eq!(read(&helper.target), "new exec");
        assert_eq!(read(&bob.target), "new bob");
        assert_eq!(read(&bob.aside), "old bob");
        assert!(!helper.staged.exists() && !bob.staged.exists());
    }

    #[test]
    fn failed_install_restores_the_previous_pair() {
        let directory = TestDir::new("rollback");
        let (helper, bob) = pair(&directory.0);
        std::fs::remove_file(&bob.staged).unwrap();
        assert!(install(&[&helper, &bob]).is_err());
        assert_eq!(read(&helper.target), "old exec");
        assert_eq!(read(&bob.target), "old bob");
        assert!(!helper.aside.exists() && !bob.aside.exists());
    }

    #[cfg(windows)]
    #[test]
    fn busy_helper_stops_the_update_before_bob_changes() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = TestDir::new("busy");
        let (helper, bob) = pair(&directory.0);
        let _running = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(&helper.target)
            .unwrap();
        let error = install(&[&helper, &bob]).unwrap_err();
        assert!(
            format!("{error:#}").contains("wait for it to finish"),
            "{error:#}"
        );
        assert_eq!(read(&bob.target), "old bob");
        assert_eq!(read(&bob.staged), "new bob");
    }
}
