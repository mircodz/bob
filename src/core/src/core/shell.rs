use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct BashShell {
    executable: Result<PathBuf, String>,
}

impl BashShell {
    pub fn resolve(cwd: &Path, configured: Option<&Path>) -> Self {
        let path = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        #[cfg(windows)]
        let roots = {
            let mut roots: Vec<_> = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(|value| PathBuf::from(value).join("Git"))
                .collect();
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                roots.push(PathBuf::from(local).join("Programs/Git"));
            }
            roots
        };
        #[cfg(not(windows))]
        let roots = Vec::new();
        Self::resolve_with(cwd, configured, &path, &roots)
    }

    pub fn executable(&self) -> Result<&Path, &str> {
        self.executable.as_deref().map_err(String::as_str)
    }

    fn resolve_with(
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
        Self { executable }
    }
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
fn native_windows_path(path: &Path) -> bool {
    let blocked = path.components().any(|part| {
        let part = part.as_os_str().to_string_lossy();
        ["System32", "SysWOW64", "Sysnative", "WindowsApps"]
            .iter()
            .any(|blocked| part.eq_ignore_ascii_case(blocked))
    });
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
    !blocked
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
        assert!(BashShell::resolve_with(&root, None, &path, &[])
            .executable()
            .is_ok());
        let shell = BashShell::resolve_with(&root, Some(&selected), &path, &[]);
        assert_eq!(
            shell.executable().unwrap(),
            selected.canonicalize().unwrap()
        );
        for path_override in [
            PathBuf::from("relative/bash.exe"),
            root.join("missing.exe"),
            root.to_path_buf(),
        ] {
            let shell = BashShell::resolve_with(&root, Some(&path_override), &path, &[]);
            let error = shell.executable().unwrap_err();
            assert!(error.contains("bash_executable"));
            assert!(error.contains("no fallback was attempted"));
            assert!(error.contains(&path_override.display().to_string()));
        }
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
        let shell = BashShell::resolve_with(&root, None, &["first".into(), "second".into()], &[]);
        assert_eq!(
            shell.executable().unwrap(),
            root.join("second/bash").canonicalize().unwrap()
        );
        assert!(BashShell::resolve_with(&root, None, &[], &[])
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
        let shell = BashShell::resolve_with(
            &root,
            None,
            &[root.join("Windows/System32"), git.join("cmd")],
            std::slice::from_ref(&conventional),
        );
        assert_eq!(
            shell.executable().unwrap(),
            git.join("usr/bin/bash.exe").canonicalize().unwrap()
        );
        let shell = BashShell::resolve_with(&root, None, &[], std::slice::from_ref(&conventional));
        assert_eq!(
            shell.executable().unwrap(),
            conventional.join("bin/bash.exe").canonicalize().unwrap()
        );
        assert!(
            BashShell::resolve_with(&root, None, &[root.join("Windows/System32")], &[])
                .executable()
                .unwrap_err()
                .contains("Git for Windows")
        );
    }
}
