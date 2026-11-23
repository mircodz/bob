# bob

(づ ◕‿◕ )づ

## Install

```sh
cargo install --locked --path src/bob
```

## Usage

```sh
bob login copilot
bob
bob config
bob update
```

## MCP

```sh
bob mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /tmp
bob mcp add github -e GITHUB_TOKEN=ghp_xxx -- npx -y @modelcontextprotocol/server-github
```

## LSP

```sh
bob lsp add rust --ext rs -- rust-analyzer
bob lsp add ts --ext ts,tsx --root web -- typescript-language-server --stdio
```
