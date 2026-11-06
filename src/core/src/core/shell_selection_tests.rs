use super::*;
use crate::test_support::TestDir;

fn fixture(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"not executable code; discovery must not run this").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn filesystem_discovery_defers_version_validation_until_approved_execution() {
    let root = TestDir::new();
    let old = root.join("PowerShell/6/pwsh.exe");
    let current = root.join("PowerShell/7/pwsh.exe");
    fixture(&old);
    fixture(&current);
    let shell = CommandShell::resolve_windows_with(
        None,
        &[
            old.parent().unwrap().into(),
            current.parent().unwrap().into(),
        ],
        &[],
        &[],
    );
    assert_eq!(shell.kind(), ShellKind::Pwsh);
    assert_eq!(shell.executable().unwrap(), old.canonicalize().unwrap());
}

#[test]
fn explicit_pwsh_does_not_fall_back_to_available_git() {
    let root = TestDir::new();
    let git = root.join("Git");
    fixture(&git.join("bin/bash.exe"));
    assert!(discover_windows_bash(&[], std::slice::from_ref(&git)).is_some());
    let selection = ShellSelection {
        dialect: ShellKind::Pwsh,
        executable: root.join("missing/pwsh.exe"),
    };
    let shell = CommandShell::resolve_windows_with(Some(&selection), &[], &[], &[git]);
    assert_eq!(shell.kind(), ShellKind::Pwsh);
    let error = shell.executable().unwrap_err();
    assert!(error.contains("PowerShell 7"));
    assert!(error.contains("canonicalization"));
    assert!(error.contains("no fallback was attempted"));
    assert!(!error.contains("alias"));
}

#[test]
fn store_installation_is_distinct_from_unresolved_alias_location() {
    let root = TestDir::new();
    let store = root.join("Program Files/WindowsApps/Microsoft.PowerShell_7_x64");
    let alias = root.join("LocalAppData/Microsoft/WindowsApps");
    fixture(&store.join("pwsh.exe"));
    fixture(&alias.join("pwsh.exe"));
    let mut failures = Vec::new();
    let selected = discover_pwsh(&[alias.clone(), store.clone()], &[], &mut failures);
    assert_eq!(
        selected,
        Some(store.join("pwsh.exe").canonicalize().unwrap())
    );
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("execution-alias"));
    assert!(failures[0].contains("real PowerShell 7 executable"));
    let selection = ShellSelection {
        dialect: ShellKind::Pwsh,
        executable: alias.join("pwsh.exe"),
    };
    let shell = CommandShell::resolve_windows_with(Some(&selection), &[store], &[], &[]);
    assert!(shell
        .executable()
        .unwrap_err()
        .contains("no fallback was attempted"));
}

#[test]
fn invocation_preserves_script_as_one_argument_without_rewriting() {
    for kind in [ShellKind::Bash, ShellKind::Pwsh] {
        for script in [
            "echo 'single' \"double\" `backtick`",
            "first\r\nsecond\nthird",
            "Write-Output '終🙂 café'",
            "a\\\\\"b\\",
            "",
        ] {
            let args = invocation_args(Path::new("shell 終.exe"), kind, script).unwrap();
            assert_eq!(args.len(), kind.command_args().len() + 1);
            for (actual, expected) in args.iter().zip(kind.command_args()) {
                assert_eq!(actual.as_os_str(), std::ffi::OsStr::new(*expected));
            }
            assert_eq!(
                args.last().unwrap().as_os_str(),
                std::ffi::OsStr::new(script)
            );
        }
        assert_eq!(
            invocation_args(Path::new("shell"), kind, "before\0after")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn windows_argv_length_counts_quotes_backslashes_unicode_and_terminator() {
    let wide = |text: &str| text.encode_utf16().collect::<Vec<_>>();
    for (argument, expected) in [
        ("", 2),
        ("plain", 5),
        ("two words", 11),
        ("`", 1),
        ("\"", 2),
        ("\\\"", 4),
        ("a \\", 6),
        ("終🙂", 3),
        ("a\nb", 3),
    ] {
        assert_eq!(
            windows_argument_len(&wide(argument)),
            expected,
            "{argument:?}"
        );
    }
    let executable = wide("C:\\Program Files\\終\\pwsh.exe");
    let overhead = executable.len() + 2 + 1 + 1;
    let at_limit = vec![b'x' as u16; 32767 - overhead];
    assert!(check_windows_command_line(&executable, std::slice::from_ref(&at_limit)).is_ok());
    let mut too_long = at_limit;
    too_long.push(b'x' as u16);
    assert!(check_windows_command_line(&executable, &[too_long])
        .unwrap_err()
        .to_string()
        .contains("UTF-16"));
    assert!(check_windows_command_line(&executable, &[vec![b'"' as u16; 16384]]).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_executable_paths_are_preserved() {
    use std::os::unix::ffi::OsStringExt;
    let root = TestDir::new();
    let selected = root.join(OsString::from_vec(b"shell-\xff".to_vec()));
    fixture(&selected);
    let shell = CommandShell::resolve(
        &root,
        Some(&ShellSelection {
            dialect: ShellKind::Bash,
            executable: selected.clone(),
        }),
    );
    assert_eq!(
        shell.executable().unwrap(),
        selected.canonicalize().unwrap()
    );
}

#[cfg(windows)]
#[test]
fn sharing_denial_is_reported_as_access_failure_not_an_alias() {
    use std::os::windows::fs::OpenOptionsExt;
    let root = TestDir::new();
    let executable = root.join("private/pwsh.exe");
    fixture(&executable);
    let _locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&executable)
        .unwrap();
    let error = validate_candidate(&executable, ShellKind::Pwsh, true).unwrap_err();
    assert!(error.contains("PowerShell 7"));
    assert!(error.contains("os error 32"), "{error}");
    assert!(!error.contains("alias"), "{error}");
}
