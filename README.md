# bob

(づ ◕‿◕ )づ

## Installation

```sh
cargo install --path src/tui
```

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