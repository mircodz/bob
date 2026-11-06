use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSelection {
    pub dialect: ShellKind,
    pub executable: PathBuf,
}

pub const PWSH_VERSION_CHECK: &str = "if ($PSVersionTable.PSEdition -eq 'Core' -and $PSVersionTable.PSVersion.Major -ge 7) { exit 0 } else { exit 125 }";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    pub fn from_resolved(executable: Result<PathBuf, String>, kind: ShellKind) -> Self {
        Self { executable, kind }
    }

    pub fn resolve(cwd: &Path, configured: Option<&ShellSelection>) -> Self {
        if let Some(selection) = configured {
            return Self {
                executable: validate_override(&selection.executable, selection.dialect),
                kind: selection.dialect,
            };
        }
        #[cfg(windows)]
        {
            let pwsh_roots: Vec<_> = ["ProgramW6432", "ProgramFiles"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(|value| PathBuf::from(value).join("PowerShell/7"))
                .collect();
            let _ = cwd;
            Self::resolve_windows_with(None, &search_path(), &pwsh_roots, &git_roots())
        }
        #[cfg(not(windows))]
        Self::resolve_bash(cwd, None)
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
        configured: Option<&ShellSelection>,
        path: &[PathBuf],
        pwsh_roots: &[PathBuf],
        git_roots: &[PathBuf],
    ) -> Self {
        if let Some(selection) = configured {
            return Self {
                executable: validate_candidate(&selection.executable, selection.dialect, true)
                    .map_err(|error| format!("{error}; no fallback was attempted")),
                kind: selection.dialect,
            };
        }
        let mut failures = Vec::new();
        if let Some(executable) = discover_pwsh(path, pwsh_roots, &mut failures) {
            return Self {
                executable: Ok(executable),
                kind: ShellKind::Pwsh,
            };
        }
        let executable = discover_windows_bash_with_errors(path, git_roots, &mut failures)
            .ok_or_else(|| format!(
                "No command shell available: install PowerShell 7 (pwsh.exe) or Git for Windows, or configure [shell] dialect and an absolute executable. WSL launchers are not supported. Discovery failures: {}",
                failures.join("; ")
            ));
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
            validate_override(configured, ShellKind::Bash)
        } else {
            discover(cwd, path, git_roots).ok_or_else(|| {
                if cfg!(windows) {
                    "Bash unavailable: install Git for Windows or configure [shell] with dialect = 'bash' and an absolute executable path in ~/.bob/config.toml or .bob.config.toml. WSL launchers are not supported."
                } else {
                    "Bash unavailable: install bash on PATH or configure [shell] with dialect = 'bash' and its absolute executable path in ~/.bob/config.toml or .bob.config.toml."
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

fn validate_override(path: &Path, kind: ShellKind) -> Result<PathBuf, String> {
    validate_candidate(path, kind, cfg!(windows))
        .map_err(|error| format!("{error}; no fallback was attempted"))
}

fn validate_candidate(path: &Path, kind: ShellKind, windows: bool) -> Result<PathBuf, String> {
    let invalid = |phase: &str, reason: &dyn std::fmt::Display| {
        format!(
            "{} executable '{}' failed {phase}: {reason}",
            kind.name(),
            path.display()
        )
    };
    if !path.is_absolute() {
        return Err(invalid("selection", &"the path must be absolute"));
    }
    if windows {
        validate_windows_path(path, kind).map_err(|reason| invalid("selection", &reason))?;
    }
    let resolved =
        std::fs::canonicalize(path).map_err(|error| invalid("canonicalization", &error))?;
    if windows {
        validate_windows_path(&resolved, kind)
            .map_err(|reason| invalid("resolved target", &reason))?;
    }
    let metadata = std::fs::metadata(&resolved).map_err(|error| invalid("metadata", &error))?;
    if !metadata.is_file() {
        return Err(invalid(
            "verification",
            &"the path is not a regular executable file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(invalid(
                "verification",
                &"the file has no execute permission",
            ));
        }
    }
    std::fs::File::open(&resolved).map_err(|error| invalid("open for verification", &error))?;
    Ok(resolved)
}

fn validate_windows_path(path: &Path, kind: ShellKind) -> Result<(), &'static str> {
    if path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("WindowsApps"))
    {
        return Err("the Store execution-alias directory is unsupported; set [shell].executable to the real PowerShell 7 executable (for example under Program Files/PowerShell/7), not the WindowsApps alias");
    }
    let valid = match kind {
        ShellKind::Bash => native_windows_path(path),
        ShellKind::Pwsh => native_pwsh_path(path),
    };
    if valid {
        Ok(())
    } else {
        Err("use a native executable for the selected dialect, not a Windows/WSL launcher or command wrapper")
    }
}

#[cfg(not(windows))]
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
    discover_windows_bash_with_errors(path, git_roots, &mut Vec::new())
}

#[cfg(any(windows, test))]
fn discover_windows_bash_with_errors(
    path: &[PathBuf],
    git_roots: &[PathBuf],
    failures: &mut Vec<String>,
) -> Option<PathBuf> {
    windows_candidates(path, git_roots)
        .into_iter()
        .find_map(
            |candidate| match validate_candidate(&candidate, ShellKind::Bash, true) {
                Ok(path) => Some(path),
                Err(error) => {
                    failures.push(error);
                    None
                }
            },
        )
}

#[cfg(any(windows, test))]
fn discover_pwsh(
    path: &[PathBuf],
    roots: &[PathBuf],
    failures: &mut Vec<String>,
) -> Option<PathBuf> {
    let mut seen = Vec::new();
    path.iter()
        .chain(roots)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            let candidate = directory.join("pwsh.exe");
            if seen.contains(&candidate) {
                return None;
            }
            seen.push(candidate.clone());
            match validate_candidate(&candidate, ShellKind::Pwsh, true) {
                Ok(path) => Some(path),
                Err(error) => {
                    failures.push(error);
                    None
                }
            }
        })
}

pub fn invocation_args(
    executable: &Path,
    kind: ShellKind,
    script: &str,
) -> io::Result<Vec<OsString>> {
    if script.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shell command contains a NUL byte",
        ));
    }
    let args: Vec<OsString> = kind
        .command_args()
        .iter()
        .map(|arg| OsString::from(*arg))
        .chain(std::iter::once(OsString::from(script)))
        .collect();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let executable: Vec<u16> = executable.as_os_str().encode_wide().collect();
        let args: Vec<Vec<u16>> = args.iter().map(|arg| arg.encode_wide().collect()).collect();
        check_windows_command_line(&executable, &args)?;
    }
    #[cfg(not(windows))]
    let _ = executable;
    Ok(args)
}

#[cfg(any(windows, test))]
fn check_windows_command_line(executable: &[u16], args: &[Vec<u16>]) -> io::Result<()> {
    if executable.contains(&0) || executable.contains(&(b'"' as u16)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows shell executable path",
        ));
    }
    let length = executable.len()
        + 2
        + 1
        + args
            .iter()
            .map(|arg| 1 + windows_argument_len(arg))
            .sum::<usize>();
    if length > 32767 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("shell command line requires {length} UTF-16 code units including quoting and terminator; Windows permits at most 32767; shorten the command or use a script file")));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_argument_len(arg: &[u16]) -> usize {
    // Mirror std::process::Command escaping without changing the transported argument.
    let quoted = arg.is_empty()
        || arg
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16);
    let mut length = usize::from(quoted) * 2;
    let mut slashes = 0;
    for unit in arg {
        if *unit == b'\\' as u16 {
            slashes += 1;
        } else {
            length += slashes + 1;
            if *unit == b'"' as u16 {
                length += slashes + 1;
            }
            slashes = 0;
        }
    }
    length + slashes * if quoted { 2 } else { 1 }
}

fn blocked_windows_directory(path: &Path) -> bool {
    path.components().any(|part| {
        let part = part.as_os_str();
        ["System32", "SysWOW64", "Sysnative", "WindowsApps"]
            .iter()
            .any(|blocked| part.eq_ignore_ascii_case(blocked))
    })
}

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

fn native_windows_path(path: &Path) -> bool {
    let launcher = path.file_name().is_some_and(|name| {
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
#[path = "shell_selection_tests.rs"]
mod selection_tests;

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
            assert!(error.contains("Bash executable"));
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
            );
            assert_eq!(shell.kind(), ShellKind::Pwsh);
            assert_eq!(
                shell.executable().unwrap(),
                expected.join("pwsh.exe").canonicalize().unwrap()
            );
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
        );
        assert_eq!(shell.kind(), ShellKind::Bash);
        assert_eq!(
            shell.executable().unwrap(),
            git.join("bin/bash.exe").canonicalize().unwrap()
        );
        let shell = CommandShell::resolve_windows_with(None, &path, &[], &[]);
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
        let selection = ShellSelection {
            dialect: ShellKind::Bash,
            executable: bash.clone(),
        };
        let shell = CommandShell::resolve_windows_with(Some(&selection), &path, &[], &[]);
        assert_eq!(shell.kind(), ShellKind::Bash);
        assert_eq!(shell.executable().unwrap(), bash.canonicalize().unwrap());
        for invalid in [
            PathBuf::from("relative/bash.exe"),
            root.join("missing.exe"),
            root.to_path_buf(),
        ] {
            let selection = ShellSelection {
                dialect: ShellKind::Bash,
                executable: invalid,
            };
            let shell = CommandShell::resolve_windows_with(Some(&selection), &path, &[], &[]);
            assert_eq!(shell.kind(), ShellKind::Bash);
            assert!(shell
                .executable()
                .unwrap_err()
                .contains("no fallback was attempted"));
        }
        #[cfg(windows)]
        {
            let selection = ShellSelection {
                dialect: ShellKind::Bash,
                executable: path[0].join("pwsh.exe"),
            };
            let shell = CommandShell::resolve_windows_with(Some(&selection), &path, &[], &[]);
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
        assert!(discover_pwsh(&path, &[], &mut Vec::new()).is_none());
        let native = root.join("PowerShell/7/pwsh.exe");
        fixture_file(&native);
        let link = root.join("native-link");
        std::fs::create_dir_all(&link).unwrap();
        symlink(&native, link.join("pwsh.exe")).unwrap();
        path.push(link);
        assert_eq!(
            discover_pwsh(&path, &[], &mut Vec::new()).unwrap(),
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
        let automatic = CommandShell::resolve(
            &root,
            Some(&ShellSelection {
                dialect: ShellKind::Bash,
                executable: selected.clone(),
            }),
        );
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
