//! Config loading. Merged from three layers (later wins):
//!   1. built-in defaults
//!   2. global   — ~/.bob/config.toml
//!   3. project  — ./.bob.config.toml
//!
//! Config is TOML (human-edited, comment-friendly). Machine data — auth.json,
//! usage.jsonl, the SQLite session db — stays in its own formats.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionsConfig {
    pub default: String, // "allow" | "deny" | "ask"
    pub allow_bash: Vec<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// A language server bob should manage for this project. Unlike MCP servers
/// (global), LSP servers are configured per-project in `./.bob.config.toml`
/// because the command, extensions, and root are inherently repo-specific.
/// Monorepos list multiple entries — one per language-root pair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspServerConfig {
    /// Display name (e.g. "rust", "ts"). Namespaces the health indicator.
    pub name: String,
    /// The language-server executable (e.g. "rust-analyzer").
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions this server handles, without the dot (e.g. ["rs"]).
    pub extensions: Vec<String>,
    /// Project root the server is launched in, relative to the repo root.
    /// "." for a single-crate repo; subdirs for monorepo members.
    #[serde(default = "default_root")]
    pub root: String,
}

fn default_root() -> String {
    ".".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BobConfig {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// TUI color theme: "dark" (default), "light", or "terminal" (inherit the
    /// terminal's own ANSI palette). Unknown values fall back to dark.
    #[serde(default)]
    pub theme: Option<String>,
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub lsp_servers: Vec<LspServerConfig>,
}

impl Default for BobConfig {
    fn default() -> Self {
        BobConfig {
            provider: "anthropic".to_string(),
            system: None,
            max_turns: Some(20),
            theme: None,
            permissions: PermissionsConfig {
                default: "ask".to_string(),
                allow_bash: ["ls", "cat", "pwd", "echo", "grep", "find", "git", "bun", "node", "tsc"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                allow: ["read_file", "list_dir", "glob", "grep", "todo_write"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                deny: vec![],
            },
            mcp_servers: vec![],
            lsp_servers: vec![],
        }
    }
}

/// Partial config for merging (all fields optional).
#[derive(Debug, Deserialize)]
struct PartialConfig {
    provider: Option<String>,
    system: Option<String>,
    max_turns: Option<u32>,
    theme: Option<String>,
    permissions: Option<PermissionsConfig>,
    mcp_servers: Option<Vec<McpServerConfig>>,
    lsp_servers: Option<Vec<LspServerConfig>>,
}

fn read_partial(path: &Path) -> anyhow::Result<Option<PartialConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let parsed: PartialConfig = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("invalid config at {}: {}", path.display(), e))?;
    Ok(Some(parsed))
}

fn merge(base: BobConfig, over: PartialConfig) -> BobConfig {
    BobConfig {
        provider: over.provider.unwrap_or(base.provider),
        system: over.system.or(base.system),
        max_turns: over.max_turns.or(base.max_turns),
        theme: over.theme.or(base.theme),
        permissions: over.permissions.unwrap_or(base.permissions),
        mcp_servers: over.mcp_servers.unwrap_or(base.mcp_servers),
        lsp_servers: over.lsp_servers.unwrap_or(base.lsp_servers),
    }
}

pub fn load_config(cwd: &Path) -> anyhow::Result<BobConfig> {
    let mut cfg = BobConfig::default();
    // Global: ~/.bob/config.toml
    if let Some(home) = dirs::home_dir() {
        if let Some(user) = read_partial(&home.join(".bob").join("config.toml"))? {
            cfg = merge(cfg, user);
        }
    }
    // Project: ./.bob.config.toml
    if let Some(project) = read_partial(&cwd.join(".bob.config.toml"))? {
        cfg = merge(cfg, project);
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// MCP server management — read/write ~/.bob/config.toml in place.
//
// We edit the TOML document with toml_edit so the user's comments and the
// formatting of unrelated keys are preserved across `bob mcp add/remove`.
// ---------------------------------------------------------------------------

fn global_config_path() -> anyhow::Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".bob").join("config.toml"))
}

fn read_config_doc() -> anyhow::Result<toml_edit::DocumentMut> {
    let path = global_config_path()?;
    if !path.exists() {
        return Ok(toml_edit::DocumentMut::new());
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(text.parse::<toml_edit::DocumentMut>()?)
}

fn write_config_doc(doc: &toml_edit::DocumentMut) -> anyhow::Result<()> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// The configured MCP servers from the global config file (empty if none).
pub fn list_mcp_servers() -> anyhow::Result<Vec<McpServerConfig>> {
    let path = global_config_path()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(&path)?;
    #[derive(Deserialize, Default)]
    struct OnlyMcp {
        #[serde(default)]
        mcp_servers: Vec<McpServerConfig>,
    }
    let parsed: OnlyMcp = toml::from_str(&text).unwrap_or_default();
    Ok(parsed.mcp_servers)
}

/// Serialize the server list back into the document's `mcp_servers` key as an
/// array of tables, replacing whatever was there.
fn set_mcp_servers(
    doc: &mut toml_edit::DocumentMut,
    servers: &[McpServerConfig],
) -> anyhow::Result<()> {
    use toml_edit::{value, Array, ArrayOfTables, Item, Table};
    let mut tables = ArrayOfTables::new();
    for s in servers {
        let mut t = Table::new();
        t["name"] = value(&s.name);
        t["command"] = value(&s.command);
        if !s.args.is_empty() {
            let mut arr = Array::new();
            for a in &s.args {
                arr.push(a.as_str());
            }
            t["args"] = value(arr);
        }
        if !s.env.is_empty() {
            let mut env = Table::new();
            for (k, v) in &s.env {
                env[k] = value(v.as_str());
            }
            t["env"] = Item::Table(env);
        }
        tables.push(t);
    }
    doc["mcp_servers"] = Item::ArrayOfTables(tables);
    Ok(())
}

/// Add (or replace, by name) an MCP server in the global config file.
/// Returns true if an existing server with the same name was replaced.
pub fn add_mcp_server(server: McpServerConfig) -> anyhow::Result<bool> {
    let mut doc = read_config_doc()?;
    let mut servers = list_mcp_servers()?;
    let replaced = servers.iter().any(|s| s.name == server.name);
    servers.retain(|s| s.name != server.name);
    servers.push(server);
    set_mcp_servers(&mut doc, &servers)?;
    write_config_doc(&doc)?;
    Ok(replaced)
}

/// Remove an MCP server by name. Returns true if one was removed.
pub fn remove_mcp_server(name: &str) -> anyhow::Result<bool> {
    let mut doc = read_config_doc()?;
    let mut servers = list_mcp_servers()?;
    let before = servers.len();
    servers.retain(|s| s.name != name);
    let removed = servers.len() != before;
    set_mcp_servers(&mut doc, &servers)?;
    write_config_doc(&doc)?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// LSP server management — read/write the PROJECT config (./.bob.config.toml).
// LSP servers are per-project (command, extensions, and root are repo-specific),
// so unlike MCP these helpers operate on the project file in `cwd`, not the
// global one. Same toml_edit approach to preserve comments and unrelated keys.
// ---------------------------------------------------------------------------

fn project_config_path(cwd: &Path) -> std::path::PathBuf {
    cwd.join(".bob.config.toml")
}

fn read_project_doc(cwd: &Path) -> anyhow::Result<toml_edit::DocumentMut> {
    let path = project_config_path(cwd);
    if !path.exists() {
        return Ok(toml_edit::DocumentMut::new());
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(text.parse::<toml_edit::DocumentMut>()?)
}

fn write_project_doc(cwd: &Path, doc: &toml_edit::DocumentMut) -> anyhow::Result<()> {
    std::fs::write(project_config_path(cwd), doc.to_string())?;
    Ok(())
}

/// The configured LSP servers from the project config file (empty if none).
pub fn list_lsp_servers(cwd: &Path) -> anyhow::Result<Vec<LspServerConfig>> {
    let path = project_config_path(cwd);
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(&path)?;
    #[derive(Deserialize, Default)]
    struct OnlyLsp {
        #[serde(default)]
        lsp_servers: Vec<LspServerConfig>,
    }
    let parsed: OnlyLsp = toml::from_str(&text).unwrap_or_default();
    Ok(parsed.lsp_servers)
}

/// Serialize the server list back into the document's `lsp_servers` key as an
/// array of tables, replacing whatever was there.
fn set_lsp_servers(
    doc: &mut toml_edit::DocumentMut,
    servers: &[LspServerConfig],
) -> anyhow::Result<()> {
    use toml_edit::{value, Array, ArrayOfTables, Item, Table};
    let mut tables = ArrayOfTables::new();
    for s in servers {
        let mut t = Table::new();
        t["name"] = value(&s.name);
        t["command"] = value(&s.command);
        if !s.args.is_empty() {
            let mut arr = Array::new();
            for a in &s.args {
                arr.push(a.as_str());
            }
            t["args"] = value(arr);
        }
        let mut exts = Array::new();
        for e in &s.extensions {
            exts.push(e.as_str());
        }
        t["extensions"] = value(exts);
        t["root"] = value(&s.root);
        tables.push(t);
    }
    doc["lsp_servers"] = Item::ArrayOfTables(tables);
    Ok(())
}

/// Add (or replace, by name) an LSP server in the project config file.
/// Returns true if an existing server with the same name was replaced.
pub fn add_lsp_server(cwd: &Path, server: LspServerConfig) -> anyhow::Result<bool> {
    let mut doc = read_project_doc(cwd)?;
    let mut servers = list_lsp_servers(cwd)?;
    let replaced = servers.iter().any(|s| s.name == server.name);
    servers.retain(|s| s.name != server.name);
    servers.push(server);
    set_lsp_servers(&mut doc, &servers)?;
    write_project_doc(cwd, &doc)?;
    Ok(replaced)
}

/// Remove an LSP server by name. Returns true if one was removed.
pub fn remove_lsp_server(cwd: &Path, name: &str) -> anyhow::Result<bool> {
    let mut doc = read_project_doc(cwd)?;
    let mut servers = list_lsp_servers(cwd)?;
    let before = servers.len();
    servers.retain(|s| s.name != name);
    let removed = servers.len() != before;
    set_lsp_servers(&mut doc, &servers)?;
    write_project_doc(cwd, &doc)?;
    Ok(removed)
}

/// Persist the TUI theme name into the project config (./.bob.config.toml),
/// preserving comments and other keys. Used by the `/theme` command so a switch
/// sticks across restarts.
pub fn set_theme_in_project(cwd: &Path, theme: &str) -> anyhow::Result<()> {
    let mut doc = read_project_doc(cwd)?;
    doc["theme"] = toml_edit::value(theme);
    write_project_doc(cwd, &doc)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_config() {
        let text = r#"
            provider = "anthropic:claude-sonnet"
            max_turns = 12
            [permissions]
            default = "ask"
            allow_bash = ["ls", "git"]
            allow = ["read_file"]
            deny = []
        "#;
        let p: PartialConfig = toml::from_str(text).unwrap();
        assert_eq!(p.provider.as_deref(), Some("anthropic:claude-sonnet"));
        assert_eq!(p.max_turns, Some(12));
        assert_eq!(p.permissions.unwrap().allow_bash, vec!["ls", "git"]);
    }

    #[test]
    fn set_mcp_servers_preserves_comments() {
        // A hand-written doc with a comment and an unrelated key.
        let mut doc = "# keep me\nprovider = \"anthropic\"\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let servers = vec![McpServerConfig {
            name: "fs".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            env: std::collections::HashMap::new(),
        }];
        set_mcp_servers(&mut doc, &servers).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# keep me"), "comment must survive");
        assert!(out.contains("provider = \"anthropic\""));
        assert!(out.contains("[[mcp_servers]]"));
        assert!(out.contains("name = \"fs\""));

        // Round-trip back into a config struct.
        #[derive(Deserialize, Default)]
        struct OnlyMcp {
            #[serde(default)]
            mcp_servers: Vec<McpServerConfig>,
        }
        let parsed: OnlyMcp = toml::from_str(&out).unwrap();
        assert_eq!(parsed.mcp_servers.len(), 1);
        assert_eq!(parsed.mcp_servers[0].name, "fs");
    }

    #[test]
    fn set_lsp_servers_roundtrips_and_preserves_comments() {
        let mut doc = "# project config\nprovider = \"copilot\"\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let servers = vec![LspServerConfig {
            name: "rust".into(),
            command: "rust-analyzer".into(),
            args: vec![],
            extensions: vec!["rs".into()],
            root: ".".into(),
        }];
        set_lsp_servers(&mut doc, &servers).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# project config"), "comment must survive");
        assert!(out.contains("[[lsp_servers]]"));
        assert!(out.contains("name = \"rust\""));
        assert!(out.contains("extensions = [\"rs\"]"));

        #[derive(Deserialize, Default)]
        struct OnlyLsp {
            #[serde(default)]
            lsp_servers: Vec<LspServerConfig>,
        }
        let parsed: OnlyLsp = toml::from_str(&out).unwrap();
        assert_eq!(parsed.lsp_servers.len(), 1);
        assert_eq!(parsed.lsp_servers[0].command, "rust-analyzer");
        assert_eq!(parsed.lsp_servers[0].root, ".");
    }
}
