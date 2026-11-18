# bob

(づ ◕‿◕ )づ

## Installation

```sh
cargo install --locked --path src/bob
```

Or grab prebuilt binaries for macOS (Apple Silicon), Linux and Windows (x86_64) from the [edge release](https://github.com/mircodz/bob/releases/tag/edge), which CI rebuilds from every push to `main` that passes. Rename `bob-<platform>` and `bob-exec-<platform>` to `bob` and `bob-exec` (keeping `.exe` on Windows), put both in the same directory on your `PATH`, and on macOS or Linux make them executable with `chmod +x`. From then on, `bob update` replaces both with the newest build.

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
