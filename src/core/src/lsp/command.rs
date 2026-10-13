use std::path::{Path, PathBuf};

pub(super) fn resolve_command(command: &str) -> PathBuf {
    let path_dirs = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let install_dirs = install_dirs(
        cfg!(windows),
        dirs::home_dir().as_deref(),
        std::env::var_os("CARGO_HOME").map(PathBuf::from).as_deref(),
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .as_deref(),
    );
    resolve_with(
        command,
        cfg!(windows),
        &path_dirs,
        &install_dirs,
        std::env::current_dir().ok().as_deref(),
        is_executable,
    )
}

fn install_dirs(
    windows: bool,
    home: Option<&Path>,
    cargo_home: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(cargo_home) = cargo_home {
        dirs.push(cargo_home.join("bin"));
    } else if let Some(home) = home {
        dirs.push(home.join(".cargo").join("bin"));
    }
    if let Some(home) = home {
        if !windows {
            dirs.push(home.join(".local").join("bin"));
        }
        dirs.push(home.join("go").join("bin"));
    }
    if windows {
        if let Some(local_app_data) = local_app_data {
            dirs.push(
                local_app_data
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links"),
            );
        }
    } else {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    dirs
}

fn resolve_with(
    command: &str,
    windows: bool,
    path_dirs: &[PathBuf],
    install_dirs: &[PathBuf],
    cwd: Option<&Path>,
    executable: impl Fn(&Path) -> bool,
) -> PathBuf {
    let path = Path::new(command);
    if path.is_absolute()
        || command.contains('/')
        || (windows && (command.contains('\\') || command.as_bytes().get(1) == Some(&b':')))
    {
        return path.to_path_buf();
    }
    let names = if windows && path.extension().is_none() {
        vec![format!("{command}.exe"), command.to_string()]
    } else {
        vec![command.to_string()]
    };
    for dir in path_dirs.iter().chain(install_dirs) {
        let dir = if dir.is_absolute() {
            dir.clone()
        } else if let Some(cwd) = cwd {
            cwd.join(dir)
        } else {
            continue;
        };
        for name in &names {
            let candidate = dir.join(name);
            if executable(&candidate) {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn windows_exe_candidates_respect_path_order_before_install_fallbacks() {
        let dir = TestDir::new();
        let paths = [dir.join("first"), dir.join("second")];
        let installs = [dir.join("cargo")];
        let first = paths[0].join("rust-analyzer.exe");
        let second = paths[1].join("rust-analyzer.exe");
        let installed = installs[0].join("rust-analyzer.exe");
        for (available, expected) in [
            (
                vec![first.clone(), second.clone(), installed.clone()],
                first,
            ),
            (vec![second.clone(), installed.clone()], second),
            (vec![installed.clone()], installed),
        ] {
            for command in ["rust-analyzer", "rust-analyzer.exe"] {
                assert_eq!(
                    resolve_with(command, true, &paths, &installs, None, |candidate| {
                        available.iter().any(|path| path == candidate)
                    }),
                    expected
                );
            }
        }
    }

    #[test]
    fn explicit_windows_paths_are_preserved_without_search() {
        for command in [
            r"C:\Program Files\Lsp\server.exe",
            r"C:tools\server",
            r"C:server",
            r".\tools\server.exe",
            r"\\server\share\server.exe",
            "C:/tools/server.exe",
        ] {
            assert_eq!(
                resolve_with(command, true, &[], &[], None, |_| {
                    panic!("explicit paths must not be searched")
                }),
                PathBuf::from(command)
            );
        }
    }

    #[test]
    fn relative_path_entries_resolve_against_launch_directory() {
        let dir = TestDir::new();
        let expected = dir.join("bin").join("server.exe");
        assert_eq!(
            resolve_with(
                "server",
                true,
                &[PathBuf::from("bin")],
                &[],
                Some(dir.path()),
                |candidate| candidate == expected,
            ),
            expected
        );
        assert_eq!(
            resolve_with("missing", true, &[], &[], Some(dir.path()), |_| false),
            PathBuf::from("missing")
        );
    }

    #[test]
    fn windows_install_directories_use_native_locations_and_cargo_override() {
        let dir = TestDir::new();
        let home = dir.join("home");
        let cargo = dir.join("custom-cargo");
        let local = dir.join("local");
        assert_eq!(
            install_dirs(true, Some(&home), Some(&cargo), Some(&local)),
            vec![
                cargo.join("bin"),
                home.join("go").join("bin"),
                local.join("Microsoft").join("WinGet").join("Links"),
            ]
        );
        assert_eq!(
            install_dirs(true, Some(&home), None, None),
            vec![home.join(".cargo").join("bin"), home.join("go").join("bin")]
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn resolved_windows_executable_launches_directly() {
        let dir = TestDir::new();
        let executable = dir.join("lsp-fixture.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let resolved = resolve_with(
            "lsp-fixture",
            true,
            &[dir.path().to_path_buf()],
            &[],
            None,
            is_executable,
        );
        assert_eq!(resolved, executable);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::process::Command::new(resolved)
                .args(["--list", "--exact", "nonexistent_lsp_fixture_test"])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success());
    }
}
