# bob

(づ ◕‿◕ )づ

## Installation

```sh
cargo install --locked --path src/bob
```

Or grab prebuilt binaries for macOS (Apple Silicon), Linux and Windows (x86_64) from the [latest release](https://github.com/mircodz/bob/releases/latest). Rename `bob-<platform>` and `bob-exec-<platform>` to `bob` and `bob-exec` (keeping `.exe` on Windows), put both in the same directory on your `PATH`, and on macOS or Linux make them executable with `chmod +x`. From then on, `bob update` replaces both with the latest release.

### Releases

Every push to `main` that passes CI is published as a numbered release, `v0.1.<commits on main>`, and marked Latest. The newest 30 stay downloadable. To start a new series such as `0.2`, change `version` in the root `Cargo.toml`.

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
