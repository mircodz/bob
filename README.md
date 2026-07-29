# bob

(づ ◕‿◕ )づ

## Installation

```sh
cargo install --path src/cli
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

### Phone control

Control a bob session from the Bob Remote iOS app. Run a relay somewhere both
your laptop and phone can reach, then host the session:

```sh
bob relay --addr 0.0.0.0:8787
bob remote
```