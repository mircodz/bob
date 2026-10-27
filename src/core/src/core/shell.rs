use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    Bash,
    Pwsh,
}

impl ShellKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::Pwsh => "PowerShell 7",
        }
    }

    pub fn command_args(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["-c"],
            Self::Pwsh => &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"],
        }
    }
}

#[derive(Debug)]
pub struct CommandShell {
    executable: Result<PathBuf, String>,
    kind: ShellKind,
}

impl CommandShell {
    pub fn resolve(cwd: &Path, configured: Option<&Path>) -> Self {
        #[cfg(windows)]
        {
            let pwsh_roots: Vec<_> = ["ProgramW6432", "ProgramFiles"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(|value| PathBuf::from(value).join("PowerShell/7"))
                .collect();
            let _ = cwd;
            Self::resolve_windows_with(
                configured,
                &search_path(),
                &pwsh_roots,
                &git_roots(),
                probe_pwsh,
            )
        }
        #[cfg(not(windows))]
        Self::resolve_bash(cwd, configured)
    }

    pub fn resolve_bash(cwd: &Path, configured: Option<&Path>) -> Self {
        #[cfg(windows)]
        let roots = git_roots();
        #[cfg(not(windows))]
        let roots = Vec::new();
        Self::resolve_bash_with(cwd, configured, &search_path(), &roots)
    }

    pub fn executable(&self) -> Result<&Path, &str> {
        self.executable.as_deref().map_err(String::as_str)
    }

    pub fn kind(&self) -> ShellKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn for_test(executable: PathBuf, kind: ShellKind) -> Self {
        Self {
            executable: Ok(executable),
            kind,
        }
    }

    #[cfg(any(windows, test))]
    fn resolve_windows_with(
        configured: Option<&Path>,
        path: &[PathBuf],
        pwsh_roots: &[PathBuf],
        git_roots: &[PathBuf],
        probe: impl FnMut(&Path) -> bool,
    ) -> Self {
        let executable = if let Some(configured) = configured {
            validate_override(configured)
        } else {
            if let Some(executable) = discover_pwsh(path, pwsh_roots, probe) {
                return Self {
                    executable: Ok(executable),
                    kind: ShellKind::Pwsh,
                };
            }
            discover_windows_bash(path, git_roots).ok_or_else(|| {
                "No command shell available: install PowerShell 7 (pwsh.exe) or Git for Windows, or set bash_executable to an absolute native Bash executable path in ~/.bob/config.toml or .bob.config.toml. WSL launchers are not supported."
                    .to_string()
            })
        };
        Self {
            executable,
            kind: ShellKind::Bash,
        }
    }

    fn resolve_bash_with(
        cwd: &Path,
        configured: Option<&Path>,
        path: &[PathBuf],
        git_roots: &[PathBuf],
    ) -> Self {
        let executable = if let Some(configured) = configured {
            validate_override(configured)
        } else {
            discover(cwd, path, git_roots).ok_or_else(|| {
                if cfg!(windows) {
                    "Bash unavailable: install Git for Windows or set bash_executable to the absolute path of its bin/bash.exe in ~/.bob/config.toml or .bob.config.toml. WSL launchers are not supported."
                } else {
                    "Bash unavailable: install bash on PATH or set bash_executable to its absolute path in ~/.bob/config.toml or .bob.config.toml."
                }
                .to_string()
            })
        };
        Self {
            executable,
            kind: ShellKind::Bash,
        }
    }
}

fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default()
}

#[cfg(windows)]
fn git_roots() -> Vec<PathBuf> {
    let mut roots: Vec<_> = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|value| PathBuf::from(value).join("Git"))
        .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs/Git"));
    }
    roots
}

fn validate_override(path: &Path) -> Result<PathBuf, String> {
    let invalid = |reason: &str| {
        format!(
            "Invalid bash_executable '{}': {reason}. Set bash_executable to an absolute native Bash executable path in ~/.bob/config.toml or .bob.config.toml; no fallback was attempted.",
            path.display()
        )
    };
    if !path.is_absolute() {
        return Err(invalid("the path must be absolute"));
    }
    #[cfg(windows)]
    if !native_windows_path(path) {
        return Err(invalid(
            "use a native .exe, not a Windows/WSL launcher or command wrapper",
        ));
    }
    let resolved = std::fs::canonicalize(path).map_err(|error| invalid(&error.to_string()))?;
    #[cfg(windows)]
    if !native_windows_path(&resolved) {
        return Err(invalid(
            "the path resolves to a Windows/WSL launcher or command wrapper",
        ));
    }
    if !is_executable(&resolved) {
        return Err(invalid("the path is not an executable file"));
    }
    Ok(resolved)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(not(windows))]
fn discover(cwd: &Path, path: &[PathBuf], _: &[PathBuf]) -> Option<PathBuf> {
    path.iter().find_map(|directory| {
        let candidate = cwd.join(directory).join("bash");
        is_executable(&candidate)
            .then(|| std::fs::canonicalize(candidate).ok())
            .flatten()
    })
}

#[cfg(windows)]
fn discover(_: &Path, path: &[PathBuf], git_roots: &[PathBuf]) -> Option<PathBuf> {
    discover_windows_bash(path, git_roots)
}

#[cfg(any(windows, test))]
fn discover_windows_bash(path: &[PathBuf], git_roots: &[PathBuf]) -> Option<PathBuf> {
    windows_candidates(path, git_roots)
        .into_iter()
        .find_map(|candidate| {
            let resolved = std::fs::canonicalize(&candidate).ok()?;
            (native_windows_path(&candidate)
                && native_windows_path(&resolved)
                && is_executable(&resolved))
            .then_some(resolved)
        })
}

#[cfg(any(windows, test))]
fn discover_pwsh(
    path: &[PathBuf],
    roots: &[PathBuf],
    mut probe: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    path.iter()
        .chain(roots)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            let candidate = directory.join("pwsh.exe");
            if !native_pwsh_path(&candidate) {
                return None;
            }
            let windows_app = candidate
                .components()
                .any(|part| part.as_os_str().eq_ignore_ascii_case("WindowsApps"));
            let resolved = match std::fs::canonicalize(&candidate) {
                Ok(resolved) => resolved,
                Err(_) if windows_app => candidate,
                Err(_) => return None,
            };
            if !native_pwsh_path(&resolved)
                || (!windows_app && !is_executable(&resolved))
                || seen
                    .iter()
                    .any(|path| path.as_os_str().eq_ignore_ascii_case(resolved.as_os_str()))
            {
                return None;
            }
            seen.push(resolved.clone());
            probe(&resolved).then_some(resolved)
        })
}

#[cfg(windows)]
fn probe_pwsh(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let child = Command::new(path)
        .args(ShellKind::Pwsh.command_args())
        .arg("if ($PSVersionTable.PSEdition -eq 'Core' -and $PSVersionTable.PSVersion.Major -ge 7) { exit 0 }; exit 1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    match child {
        Ok(mut child) => wait_for_probe(&mut child, std::time::Duration::from_secs(5)),
        Err(_) => false,
    }
}

#[cfg(any(windows, all(test, unix)))]
fn wait_for_probe(child: &mut std::process::Child, timeout: std::time::Duration) -> bool {
    use std::time::{Duration, Instant};

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(
                    Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())),
                );
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(any(windows, test))]
fn blocked_windows_directory(path: &Path) -> bool {
    path.components().any(|part| {
        let part = part.as_os_str().to_string_lossy();
        ["System32", "SysWOW64", "Sysnative", "WindowsApps"]
            .iter()
            .any(|blocked| part.eq_ignore_ascii_case(blocked))
    })
}

#[cfg(any(windows, test))]
fn native_pwsh_path(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|part| {
            ["System32", "SysWOW64", "Sysnative"]
                .iter()
                .any(|blocked| part.as_os_str().eq_ignore_ascii_case(blocked))
        })
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("pwsh.exe"))
}

#[cfg(any(windows, test))]
fn native_windows_path(path: &Path) -> bool {
    let launcher = path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        [
            "wsl.exe",
            "wslhost.exe",
            "ubuntu.exe",
            "debian.exe",
            "cmd.exe",
            "powershell.exe",
            "pwsh.exe",
        ]
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
    });
    !blocked_windows_directory(path)
        && !launcher
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
}

#[cfg(any(windows, test))]
fn windows_candidates(path: &[PathBuf], git_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for directory in path.iter().filter(|directory| directory.is_absolute()) {
        let mut root = directory.as_path();
        if directory.file_name().is_some_and(|name| {
            name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("cmd")
        }) {
            root = directory.parent().unwrap();
            if root.file_name().is_some_and(|name| {
                name.eq_ignore_ascii_case("usr")
                    || name.eq_ignore_ascii_case("mingw64")
                    || name.eq_ignore_ascii_case("mingw32")
            }) {
                root = root.parent().unwrap();
            }
        }
        if root.join("cmd/git.exe").is_file() || root.join("bin/git.exe").is_file() {
            roots.push(root.to_path_buf());
        }
    }
    roots.extend(git_roots.iter().filter(|root| root.is_absolute()).cloned());
    roots
        .into_iter()
        .flat_map(|root| [root.join("bin/bash.exe"), root.join("usr/bin/bash.exe")])
        .filter(|candidate| native_windows_path(candidate))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    fn fixture_file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn override_wins_and_invalid_override_never_falls_back() {
        let root = TestDir::new();
        let selected = root.join("custom shell 終/bash.exe");
        fixture_file(&selected);
        let fallback = root.join("fallback Git");
        fixture_file(&fallback.join("cmd/git.exe"));
        fixture_file(&fallback.join("bin/bash.exe"));
        fixture_file(&fallback.join("bin/bash"));
        let path = [fallback.join("bin")];
        assert!(CommandShell::resolve_bash_with(&root, None, &path, &[])
            .executable()
            .is_ok());
        let shell = CommandShell::resolve_bash_with(&root, Some(&selected), &path, &[]);
        assert_eq!(
            shell.executable().unwrap(),
            selected.canonicalize().unwrap()
        );
        for path_override in [
            PathBuf::from("relative/bash.exe"),
            root.join("missing.exe"),
            root.to_path_buf(),
        ] {
            let shell = CommandShell::resolve_bash_with(&root, Some(&path_override), &path, &[]);
            let error = shell.executable().unwrap_err();
            assert!(error.contains("bash_executable"));
            assert!(error.contains("no fallback was attempted"));
            assert!(error.contains(&path_override.display().to_string()));
        }
    }

    #[test]
    fn windows_prefers_pwsh_path_order_then_installation_before_git() {
        let root = TestDir::new();
        let git = root.join("Git");
        let first = root.join("Portable PowerShell 終");
        let second = root.join("Other PowerShell");
        let conventional = root.join("Program Files/PowerShell/7");
        fixture_file(&git.join("cmd/git.exe"));
        fixture_file(&git.join("bin/bash.exe"));
        for directory in [&first, &second, &conventional] {
            fixture_file(&directory.join("pwsh.exe"));
        }
        for (path, expected) in [
            (vec![git.join("cmd"), first.clone(), second.clone()], &first),
            (vec![second.clone(), first.clone()], &second),
            (vec![git.join("cmd")], &conventional),
        ] {
            let shell = CommandShell::resolve_windows_with(
                None,
                &path,
                std::slice::from_ref(&conventional),
                std::slice::from_ref(&git),
                |_| true,
            );
            assert_eq!(shell.kind(), ShellKind::Pwsh);
            assert_eq!(
                shell.executable().unwrap(),
                expected.join("pwsh.exe").canonicalize().unwrap()
            );
        }
    }

    #[test]
    fn windows_rejects_old_pwsh_once_then_selects_valid_pwsh_or_git() {
        let root = TestDir::new();
        let old = root.join("PowerShell/6");
        let current = root.join("PowerShell/7");
        let git = root.join("Git");
        fixture_file(&old.join("pwsh.exe"));
        fixture_file(&current.join("pwsh.exe"));
        fixture_file(&git.join("bin/bash.exe"));
        let old_exe = old.join("pwsh.exe").canonicalize().unwrap();
        let current_exe = current.join("pwsh.exe").canonicalize().unwrap();
        for accept_current in [true, false] {
            let mut probed = Vec::new();
            let shell = CommandShell::resolve_windows_with(
                None,
                &[old.clone(), old.clone()],
                &[old.clone(), current.clone(), current.clone()],
                std::slice::from_ref(&git),
                |candidate| {
                    probed.push(candidate.to_path_buf());
                    accept_current && candidate == current_exe
                },
            );
            assert_eq!(probed, [old_exe.clone(), current_exe.clone()]);
            if accept_current {
                assert_eq!(shell.kind(), ShellKind::Pwsh);
                assert_eq!(shell.executable().unwrap(), current_exe);
            } else {
                assert_eq!(shell.kind(), ShellKind::Bash);
                assert_eq!(
                    shell.executable().unwrap(),
                    git.join("bin/bash.exe").canonicalize().unwrap()
                );
            }
        }
    }

    #[test]
    fn windows_store_pwsh_and_unresolved_alias_require_successful_probe() {
        let root = TestDir::new();
        let store =
            root.join("Program Files/WindowsApps/Microsoft.PowerShell_7.6.0.0_x64__8wekyb3d8bbwe");
        let alias = root.join("LocalAppData/Microsoft/WindowsApps");
        fixture_file(&store.join("pwsh.exe"));
        assert!(alias.join("pwsh.exe").canonicalize().is_err());
        for directory in [&store, &alias] {
            let candidate = directory.join("pwsh.exe");
            let expected = candidate.canonicalize().unwrap_or(candidate);
            for valid in [false, true] {
                let mut probed = Vec::new();
                let selected =
                    discover_pwsh(&[directory.clone(), directory.clone()], &[], |path| {
                        probed.push(path.to_path_buf());
                        valid
                    });
                assert_eq!(probed, std::slice::from_ref(&expected));
                assert_eq!(selected, valid.then(|| expected.clone()));
            }
            assert!(!native_windows_path(&expected));
            assert!(!native_windows_path(&directory.join("bash.exe")));
        }
        fixture_file(&store.join("cmd/git.exe"));
        fixture_file(&store.join("bin/bash.exe"));
        assert!(discover_windows_bash(&[], &[store]).is_none());
        assert!(!native_windows_path(&root.join("Git/bin/wsl.exe")));
        assert!(!native_pwsh_path(&root.join("PowerShell/7/powershell.exe")));
        assert!(!native_pwsh_path(Path::new("relative/pwsh.exe")));
    }

    #[cfg(unix)]
    #[test]
    fn probe_wait_checks_exit_status_and_reaps_timed_out_child() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let root = TestDir::new();
        let fixture = root.join("probe.sh");
        std::fs::write(&fixture, "exec sleep 30\n").unwrap();
        let mut child = Command::new("/bin/sh")
            .arg(&fixture)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        assert!(!wait_for_probe(&mut child, Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(child.try_wait().unwrap().unwrap().signal(), Some(9));
        for (script, expected) in [("exit 0", true), ("exit 1", false)] {
            let mut child = Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            assert_eq!(wait_for_probe(&mut child, Duration::from_secs(5)), expected);
        }
    }

    #[test]
    fn windows_skips_unsafe_pwsh_candidates_and_falls_back_to_git() {
        let root = TestDir::new();
        let git = root.join("Git");
        fixture_file(&git.join("cmd/git.exe"));
        fixture_file(&git.join("bin/bash.exe"));
        let mut path = Vec::new();
        for directory in ["System32", "SysWOW64", "Sysnative"] {
            let directory = root.join(directory);
            fixture_file(&directory.join("pwsh.exe"));
            path.push(directory);
        }
        let wrapper = root.join("wrapper");
        fixture_file(&wrapper.join("pwsh.cmd"));
        fixture_file(&wrapper.join("powershell.exe"));
        path.push(wrapper);
        let directory = root.join("not-a-file");
        std::fs::create_dir_all(directory.join("pwsh.exe")).unwrap();
        path.push(directory);
        path.push("relative/PowerShell/7".into());
        let shell = CommandShell::resolve_windows_with(
            None,
            &path,
            &["relative/PowerShell/7".into()],
            std::slice::from_ref(&git),
            |_| panic!("unsafe candidates must not be probed"),
        );
        assert_eq!(shell.kind(), ShellKind::Bash);
        assert_eq!(
            shell.executable().unwrap(),
            git.join("bin/bash.exe").canonicalize().unwrap()
        );
        let shell = CommandShell::resolve_windows_with(None, &path, &[], &[], |_| {
            panic!("unsafe candidates must not be probed")
        });
        let error = shell.executable().unwrap_err();
        assert!(error.contains("PowerShell 7"));
        assert!(error.contains("Git for Windows"));
    }

    #[test]
    fn windows_override_forces_bash_and_fails_closed_with_pwsh_available() {
        let root = TestDir::new();
        let bash = root.join("custom/bash.exe");
        let pwsh = root.join("PowerShell/7");
        fixture_file(&bash);
        fixture_file(&pwsh.join("pwsh.exe"));
        let path = [pwsh];
        let shell = CommandShell::resolve_windows_with(Some(&bash), &path, &[], &[], |_| {
            panic!("Bash overrides must not probe PowerShell")
        });
        assert_eq!(shell.kind(), ShellKind::Bash);
        assert_eq!(shell.executable().unwrap(), bash.canonicalize().unwrap());
        for invalid in [
            PathBuf::from("relative/bash.exe"),
            root.join("missing.exe"),
            root.to_path_buf(),
        ] {
            let shell = CommandShell::resolve_windows_with(Some(&invalid), &path, &[], &[], |_| {
                panic!("invalid overrides must not fall back")
            });
            assert_eq!(shell.kind(), ShellKind::Bash);
            assert!(shell
                .executable()
                .unwrap_err()
                .contains("no fallback was attempted"));
        }
        #[cfg(windows)]
        {
            let shell = CommandShell::resolve_windows_with(
                Some(&path[0].join("pwsh.exe")),
                &path,
                &[],
                &[],
                |_| panic!("invalid overrides must not fall back"),
            );
            assert!(shell
                .executable()
                .unwrap_err()
                .contains("no fallback was attempted"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn pwsh_discovery_validates_symlink_targets_and_execute_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = TestDir::new();
        let blocked = root.join("System32/pwsh.exe");
        let wrapper = root.join("wrapper/pwsh.cmd");
        fixture_file(&blocked);
        fixture_file(&wrapper);
        let mut path = Vec::new();
        for (index, target) in [&blocked, &wrapper].into_iter().enumerate() {
            let directory = root.join(format!("link-{index}"));
            std::fs::create_dir_all(&directory).unwrap();
            symlink(target, directory.join("pwsh.exe")).unwrap();
            path.push(directory);
        }
        let nonexecutable = root.join("nonexecutable");
        fixture_file(&nonexecutable.join("pwsh.exe"));
        std::fs::set_permissions(
            nonexecutable.join("pwsh.exe"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        path.push(nonexecutable);
        assert!(discover_pwsh(&path, &[], |_| panic!("invalid target was probed")).is_none());
        let native = root.join("PowerShell/7/pwsh.exe");
        fixture_file(&native);
        let link = root.join("native-link");
        std::fs::create_dir_all(&link).unwrap();
        symlink(&native, link.join("pwsh.exe")).unwrap();
        path.push(link);
        assert_eq!(
            discover_pwsh(&path, &[], |_| true).unwrap(),
            native.canonicalize().unwrap()
        );
    }

    #[test]
    fn git_candidates_follow_path_order_then_conventional_roots() {
        let root = TestDir::new();
        let first = root.join("Portable Git 終");
        let second = root.join("Other Git");
        fixture_file(&first.join("cmd/git.exe"));
        fixture_file(&second.join("bin/git.exe"));
        let conventional = root.join("Program Files/Git");
        let candidates = windows_candidates(
            &[
                root.join("unrelated/bin"),
                first.join("cmd"),
                second.join("usr/bin"),
            ],
            std::slice::from_ref(&conventional),
        );
        assert_eq!(
            candidates,
            [
                first.join("bin/bash.exe"),
                first.join("usr/bin/bash.exe"),
                second.join("bin/bash.exe"),
                second.join("usr/bin/bash.exe"),
                conventional.join("bin/bash.exe"),
                conventional.join("usr/bin/bash.exe"),
            ]
        );
    }

    #[test]
    fn windows_discovery_excludes_launchers_and_command_wrappers() {
        let root = TestDir::new();
        for path in [
            "Windows/System32/bash.exe",
            "Windows/sYsNaTiVe/bash.exe",
            "Microsoft/WindowsApps/bash.exe",
            "Git/bin/bash.cmd",
            "Git/bin/wsl.exe",
            "Git/bin/pwsh.exe",
        ] {
            assert!(!native_windows_path(&root.join(path)), "{path}");
        }
        assert!(native_windows_path(
            &root.join("Program Files/Git/bin/bash.EXE")
        ));
        fixture_file(&root.join("WindowsApps/Git/cmd/git.exe"));
        assert!(windows_candidates(&[root.join("WindowsApps/Git/cmd")], &[]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_search_skips_nonexecutables_and_anchors_relative_entries() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestDir::new();
        fixture_file(&root.join("first/bash"));
        std::fs::set_permissions(
            root.join("first/bash"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fixture_file(&root.join("second/bash"));
        fixture_file(&root.join("first/pwsh.exe"));
        let shell =
            CommandShell::resolve_bash_with(&root, None, &["first".into(), "second".into()], &[]);
        assert_eq!(shell.kind(), ShellKind::Bash);
        assert_eq!(
            shell.executable().unwrap(),
            root.join("second/bash").canonicalize().unwrap()
        );
        let selected = root.join("second/bash");
        let automatic = CommandShell::resolve(&root, Some(&selected));
        let explicit = CommandShell::resolve_bash(&root, Some(&selected));
        assert_eq!(automatic.kind(), ShellKind::Bash);
        assert_eq!(automatic.executable(), explicit.executable());
        assert!(CommandShell::resolve_bash_with(&root, None, &[], &[])
            .executable()
            .unwrap_err()
            .contains("install bash"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefers_git_on_path_over_conventional_installations() {
        let root = TestDir::new();
        let git = root.join("portable git 終");
        let conventional = root.join("Program Files/Git");
        fixture_file(&git.join("cmd/git.exe"));
        fixture_file(&git.join("usr/bin/bash.exe"));
        fixture_file(&conventional.join("bin/bash.exe"));
        fixture_file(&root.join("Windows/System32/bash.exe"));
        let shell = CommandShell::resolve_bash_with(
            &root,
            None,
            &[root.join("Windows/System32"), git.join("cmd")],
            std::slice::from_ref(&conventional),
        );
        assert_eq!(
            shell.executable().unwrap(),
            git.join("usr/bin/bash.exe").canonicalize().unwrap()
        );
        let shell =
            CommandShell::resolve_bash_with(&root, None, &[], std::slice::from_ref(&conventional));
        assert_eq!(
            shell.executable().unwrap(),
            conventional.join("bin/bash.exe").canonicalize().unwrap()
        );
        assert!(CommandShell::resolve_bash_with(
            &root,
            None,
            &[root.join("Windows/System32")],
            &[]
        )
        .executable()
        .unwrap_err()
        .contains("Git for Windows"));
    }
}
