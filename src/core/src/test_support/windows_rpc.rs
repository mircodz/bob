use super::TestDir;
use crate::core::shell::{CommandShell, ShellKind};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::Duration;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

pub(crate) struct Server {
    pub command: String,
    pub args: Vec<String>,
    pub root: PathBuf,
}

impl Server {
    pub fn new(dir: &TestDir, framed: bool) -> Self {
        let root = dir.join("RPC workspace 東京 with spaces");
        std::fs::create_dir(&root).unwrap();
        let script = root.join("stdio server 日本語.ps1");
        std::fs::write(&script, SCRIPT).unwrap();
        let selection = std::env::var_os("BOB_NATIVE_PWSH").map(|executable| {
            crate::core::shell::ShellSelection {
                dialect: ShellKind::Pwsh,
                executable: executable.into(),
            }
        });
        let shell = CommandShell::resolve(&root, selection.as_ref());
        assert_eq!(
            shell.kind(),
            ShellKind::Pwsh,
            "native RPC acceptance requires PowerShell 7; no Bash substitution"
        );
        Self {
            command: shell
                .executable()
                .expect("PowerShell 7 must be installed")
                .to_str()
                .unwrap()
                .into(),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-File".into(),
                script.to_str().unwrap().into(),
                if framed { "lsp" } else { "mcp" }.into(),
            ],
            root,
        }
    }
}

pub(crate) struct Process(OwnedHandle);
impl Process {
    pub fn open(pid: u32) -> Self {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(!handle.is_null(), "{}", std::io::Error::last_os_error());
        Self(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    pub async fn wait_exit(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut code = 0;
                assert_ne!(
                    unsafe { GetExitCodeProcess(self.0.as_raw_handle(), &mut code) },
                    0
                );
                if code != 259 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("RPC server process must exit when its owner disconnects");
    }
}

const SCRIPT: &str = r#"param([string]$Protocol)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSEdition -ne 'Core' -or $PSVersionTable.PSVersion.Major -lt 7) { exit 125 }
$utf8 = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8
$inputStream = [Console]::OpenStandardInput()
$outputStream = [Console]::OpenStandardOutput()
function Read-ProtocolLine {
    $bytes = [System.Collections.Generic.List[byte]]::new()
    while ($true) {
        $value = $inputStream.ReadByte()
        if ($value -lt 0) { return $null }
        if ($value -eq 10) { return $utf8.GetString($bytes.ToArray()).TrimEnd([char]13) }
        $bytes.Add([byte]$value)
    }
}
while ($true) {
    if ($Protocol -eq 'lsp') {
        $length = $null
        while ($true) {
            $line = Read-ProtocolLine
            if ($null -eq $line) { exit 0 }
            if ($line -eq '') { break }
            if ($line.StartsWith('Content-Length:')) { $length = [int]$line.Substring(15).Trim() }
        }
        if ($null -eq $length -or $length -gt 1048576) { exit 126 }
        $bytes = [byte[]]::new($length)
        $offset = 0
        while ($offset -lt $length) {
            $count = $inputStream.Read($bytes, $offset, $length - $offset)
            if ($count -eq 0) { exit 0 }
            $offset += $count
        }
        $line = $utf8.GetString($bytes)
    } else {
        $line = Read-ProtocolLine
        if ($null -eq $line) { exit 0 }
    }
    $request = ConvertFrom-Json -InputObject $line -AsHashtable
    if (-not $request.ContainsKey('id')) { continue }
    if ($request.method -eq 'fixture/stall') { [System.Threading.Thread]::Sleep(60000); continue }
    if ($request.method -eq 'fixture/malformed') {
        $bad = $utf8.GetBytes("not JSON`n")
        $outputStream.Write($bad, 0, $bad.Length)
        $outputStream.Flush()
        continue
    }
    $result = if ($request.method -eq 'initialize') {
        @{capabilities = @{positionEncoding = 'utf-16'}}
    } else {
        @{pid = $PID; cwd = (Get-Location).Path; echo = $request.params; text = "日本語 😀`nnext"; environment = $env:BOB_RPC_FIXTURE}
    }
    $response = if ($request.method -eq 'fixture/error') {
        @{jsonrpc = '2.0'; id = $request.id; error = @{code = -32602; message = 'fixture rejection'}}
    } else {
        @{jsonrpc = '2.0'; id = $request.id; result = $result}
    }
    $body = $utf8.GetBytes((ConvertTo-Json -InputObject $response -Compress -Depth 20))
    if ($Protocol -eq 'lsp') {
        $header = [System.Text.Encoding]::ASCII.GetBytes("Content-Length: $($body.Length)`r`n`r`n")
        $outputStream.Write($header, 0, $header.Length)
        $outputStream.Write($body, 0, $body.Length)
    } else {
        $outputStream.Write($body, 0, $body.Length)
        $outputStream.WriteByte(10)
    }
    $outputStream.Flush()
}
"#;
