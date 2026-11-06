# bob

(づ ◕‿◕ )づ

## Installation

```sh
cargo install --locked --path src/bob
```

This automatically installs both the `bob` frontend and its required `bob-exec` helper. You only launch `bob`; it starts the helper for shell and file execution. Keep both executables in the same directory; installing only `--bin bob` is incomplete. Release archives named `bob-<target>.tar.gz` (Unix) or `bob-<target>.zip` (Windows) contain exactly these two executables, with `.exe` suffixes on Windows. The Cargo package and library are both `bob`; no old executable aliases or legacy release archives are provided.

For local development, `cargo build --locked` builds both binaries and `cargo run --locked` starts `bob`. The workspace default is `src/bob`; the kernel/application/shared-UI directories remain separate.

### Replacing an existing installation and preserving history

If Cargo reports that an older package owns `bob` or its helper, explicitly replace those installed binaries:

```sh
cargo install --locked --path src/bob --force
```

This is a breaking package and executable cutover, not a history migration. Fresh durable sessions default to `~/.bob/sessions.sqlite`. Existing durable stores under `~/.bob-lab` are detected only to require an explicit `--database` choice; they are never automatically selected, migrated or deleted. To continue using an existing durable store, select its actual path explicitly, for example:

```sh
bob --database "$HOME/.bob-lab/sessions.sqlite" --resume
```

To choose the new store instead when an old store is detected, pass `--database "$HOME/.bob/sessions.sqlite"` explicitly. Legacy history at `~/.bob/bob.db` and configuration/provider authentication under `~/.bob` remain untouched. Never point `bob --database` at the legacy database. Environment settings now use `BOB_*` names, such as `BOB_EXEC_HELPER`; old prefixed settings are not compatibility aliases.

### First run

```sh
bob login copilot
bob
bob config
```

### MCP servers

```sh
bob mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /tmp
bob mcp add github -e GITHUB_TOKEN=ghp_xxx -- npx -y @modelcontextprotocol/server-github
```

### Language servers

```sh
bob lsp add rust --ext rs -- rust-analyzer
bob lsp add ts --ext ts,tsx --root web -- typescript-language-server --stdio
```

## Local replacement qualification

Run `cargo fmt --all --check`, `cargo build --locked --workspace --all-targets`, `cargo test --locked --workspace --all-targets`, and `cargo clippy --locked --workspace --all-targets -- -D warnings`. Build both durable binaries before helper acceptance. Run optional tests by name; never blanket-run all ignored tests because some are private subprocess/crash fixtures.

For an extracted macOS release package, with existing Copilot authentication:

```sh
python3 scripts/macos-replacement-acceptance.py --package /absolute/extracted-package --model copilot:ADVERTISED_MODEL_ID
BOB_REPLACEMENT_PACKAGE_DIR=/absolute/extracted-package cargo test --locked --release -p bob --test packaged_unix_acceptance packaged_unix_frontend_and_helper_acceptance -- --exact --nocapture
```

The PTY harness uses private synthetic sessions and records package hashes, functional assertions and bounded latency samples. First-editable-input and provider-ready times are distinct; draft input must survive initialization. It does not certify real desktop clipboard/IME or visual emulator fidelity. A legacy PTY comparison is deliberately not run because legacy cannot isolate its database while using the existing authentication home. See `ROADMAP.md` for current evidence and remaining gates; component measurements are not an end-to-end speedup claim.

Intentional differences from legacy remain: separate durable history, bounded no-follow search with explicit coverage/continuation, stricter prepared-path/unknown-outcome safeguards, and accept-edits limited to file creation/write/edit tools rather than arbitrary mutators. Remembered Bash program grants require freshly validated simple literal commands; they do not pin executable identity or sandbox arguments. PowerShell, compound/dynamic syntax and mandatory approvals are not covered by those grants.

## Native Windows qualification

The target of this hardening pass is **Windows x86-64, Windows Terminal, PowerShell 7 and Git for Windows Bash, with a local durable database**. The automated gates below do not establish full native acceptance. Native results must identify the tested source, OS build, executable hashes and package. Windows ARM64, unresolved Store execution aliases, arbitrary interior reparse-point traversal and network-hosted durable databases (including SQLite over SMB) are not qualified by this pass.

### Explicit tool shell

Set the shell in `~/.bob/config.toml` or the nearer `.bob.config.toml`:

```toml
[shell]
dialect = "pwsh"
executable = 'C:\Program Files\PowerShell\7\pwsh.exe'
```

For Git Bash:

```toml
[shell]
dialect = "bash"
executable = 'C:\Program Files\Git\usr\bin\bash.exe'
```

The dialect and executable are one setting; a nearer configuration replaces both. An explicit selection never falls back to another shell. Automatic Windows discovery prefers native PowerShell, then Git Bash. The old top-level `bash_executable` selects Bash and cannot coexist with `[shell]` in the same file. PowerShell commands are not authorized by Bash allowlists. Shell discovery does not activate Store aliases or run a version probe; version validation happens inside approved execution. Select a real executable when an alias is rejected.

Path validation distinguishes ordinary absolute drive/UNC paths and their supported extended-length forms from ambiguous drive-relative/root-relative paths, device namespaces, alternate data streams and invalid components. Interior junction/reparse traversal remains fail-closed; launching from a junction may resolve the physical workspace root. Workspace UNC qualification is not network database qualification. Keep the database local, do not rename a live database or its lock/WAL files, and do not hard-link it: hard-linked databases are rejected rather than giving different pathnames independent execution owners.

Windows replacement remains atomic through `ReplaceFileW`, preserving its metadata and recovery-backup behavior. Staging is pinned and cleanup is handle-bound, but publication must release the pin before that pathname-based API can run. Concurrent substitution in that final window is detected only after publication and reported as an unknown outcome with recovery files retained. This is a remaining race, not a prevention guarantee; do not automatically retry uncertain edits or treat this pass as complete filesystem-race protection.

### Two-binary durable installation

The application is packaged as **both** `bob.exe` and `bob-exec.exe`; extract and retain them as siblings in the same directory. From source:

```powershell
cargo build --locked --release -p bob --bins
```

Do not copy only `bob.exe`. `BOB_EXEC_HELPER`, if deliberately used, must be an absolute helper path; normal installation uses sibling discovery. The package gate removes this override and relocates its test-only driver beside the extracted helper, outside the checkout in a spaced/Unicode directory. No general test execution mode is added to shipping binaries.

### Reproducible automated gates

Use an isolated native Windows x86-64 machine with the toolchain declared in `rust-toolchain.toml`, PowerShell 7 and Git for Windows already installed. No model credentials are needed. The runner does not install dependencies or dispatch CI.

```powershell
./scripts/windows-acceptance.ps1
cargo build --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace

# After creating the actual two-binary release archive:
./scripts/windows-acceptance.ps1 -Package ./bob-x86_64-pc-windows-msvc.zip -Artifacts ./native-package-artifacts
```

For a nonstandard shell installation, set `BOB_NATIVE_PWSH` and `BOB_NATIVE_BASH` to the actual executable paths before running. These are test-runner selections, not production configuration. The lifecycle matrix explicitly uses Git's `usr/bin/bash.exe` so process-image assertions identify the real Bash process; automatic discovery through Git's `bin/bash.exe` wrapper still needs separate native qualification and is not silently treated as the same image.

The runner records tool/source identity, uses locked Cargo commands, requires nonzero passing-test counts, and starts commands suspended in a kill-on-close Windows Job Object before allowing them to run. Watchdogs detect hangs, not performance regressions. Per-run evidence under the selected artifact directory contains bounded logs, hashes and allowlisted synthetic failure markers, not copied authentication files or user conversations. Temporary extracted packages are removed and owned process trees are terminated on success and failure.

The real-helper tests cover both explicit shells: cwd and Unicode output, exit status, cancellation, timeout, dropped futures, parent/helper hard death, descendant cleanup, exactly-once synthetic effects, missing/corrupt helper and changed-image rejection. The durable recovery integration approves real helper work, rejects another execution owner and unsupported hard-link aliases, kills the owner, checks descendant exit, reopens an unknown outcome, rejects unsafe resume, and resolves without replay. The packaged driver checks denial has zero effects and approved foreground/background work uses the extracted helper. A `--version` check alone is **not** an execution pass.

### Unix relocated package gate

On a runnable native macOS or Linux target, extract the actual `bob-<target>.tar.gz` and run the relocated real-helper acceptance against it:

```sh
BOB_REPLACEMENT_PACKAGE_DIR=/absolute/path/to/extracted-package \
  cargo test --locked --release -p bob --test packaged_unix_acceptance \
  packaged_unix_frontend_and_helper_acceptance -- --exact --nocapture --test-threads=1
```

The test expects `bob` plus `bob-exec`, copies them outside the checkout into a spaced/Unicode directory, and exercises helper approval, denial, foreground/background work and missing-helper rejection with isolated configuration. CI/release run it only on native x86-64 Linux and ARM64 macOS matrix legs, not cross-compiled ARM64 Linux. This does not qualify interactive terminal behavior or native Windows.

### Measurements, not timing thresholds

```powershell
./scripts/windows-acceptance.ps1 -Measurements
# Or run the measurements individually:
cargo test --locked --release -p bob-kernel --lib long_history_reads_are_bounded -- --ignored --nocapture --test-threads=1
cargo test --locked --release -p bob --bin bob cached_composer_live_frame_measurement -- --ignored --nocapture --test-threads=1
# While exercising an identified packaged TUI process:
Get-Process -Id <bob-process-id> | Select-Object Id, WorkingSet64, PrivateMemorySize64, HandleCount, CPU
```

Repeat measurements on the identified Windows machine, retaining distributions rather than comparing one hosted-runner duration to a fixed threshold. The history test asserts structural query/page bounds; frame measurements use a synthetic rendering backend, not Windows Terminal. Large-history resume/scrolling, long streaming content, many agents, input during tool output, memory and handle trends still need representative interactive measurements. Mac timings are not Windows evidence.

### Separate packaged application / live release gate

**These items are not passed merely because CI or the helper runner is green.** Record pass/fail/not-run with the source SHA and archive hashes:

1. Run the actual extracted `bob.exe` in Windows Terminal using both PowerShell and Git Bash terminal profiles and tool-execution selections, including automatic Git wrapper discovery.
2. Exercise typing, Enter, multiline paste, supplementary Unicode, IME composition, keyboard layouts, held/repeated keys and Windows Terminal shortcuts. Verify paste cannot submit or approve work unexpectedly. Locked Crossterm's native paste and Unicode-key behavior remains an open qualification limitation; synthetic ConPTY text is not proof of paste/IME safety.
3. Exercise permission focus, numeric/default/deny actions, mouse, resize and small terminals while an isolated child runs. In an isolated desktop, check real Unicode clipboard transfer, contention and truthful failure reporting.
4. Execute and cancel packaged helper work; check terminal mode restoration after normal exit and failures, then reopen the durable session without replaying an uncertain effect.
5. Measure the large-session workloads above, including process handles and memory.
6. Run an explicitly authorized live coding scenario using **GitHub Copilot only**. Never fall back to direct-billing providers. Keep real credentials and conversations out of uploaded evidence.

Release ownership belongs to the repository maintainer. The workflow waits for every build/package leg, including Windows extracted-helper smoke, before publication. The maintainer must configure required reviewers for the `native-windows-release` GitHub environment; merely naming an environment does not protect it. After completing and recording the checklist for the exact source/archive, set the environment variable **`NATIVE_WINDOWS_ACCEPTED_SHA`** (GitHub Actions configuration variable, not a secret) to the full tested commit SHA and approve the environment deployment. Publication of **all platform archives**, not just Windows, explicitly fails if the value is absent or differs from `github.sha`. There is no reusable acceptance boolean and no native acceptance SHA is supplied by these changes. Configuring remote settings, running workflows, publishing and performing live acceptance require separate authorization.
