//! Config loading. Merged from three layers (later wins):
//!   1. built-in defaults
//!   2. user global   — ~/.bob/settings.json
//!   3. project        — ./.bob/config.json  (legacy ./bob.config.json accepted)

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BobConfig {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

impl Default for BobConfig {
    fn default() -> Self {
        BobConfig {
            provider: "anthropic".to_string(),
            system: None,
            max_turns: Some(20),
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
        }
    }
}

/// Partial config for merging (all fields optional).
#[derive(Debug, Deserialize)]
struct PartialConfig {
    provider: Option<String>,
    system: Option<String>,
    max_turns: Option<u32>,
    permissions: Option<PermissionsConfig>,
    mcp_servers: Option<Vec<McpServerConfig>>,
}

fn read_partial(path: &Path) -> anyhow::Result<Option<PartialConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let parsed: PartialConfig = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("invalid config at {}: {}", path.display(), e))?;
    Ok(Some(parsed))
}

fn merge(base: BobConfig, over: PartialConfig) -> BobConfig {
    BobConfig {
        provider: over.provider.unwrap_or(base.provider),
        system: over.system.or(base.system),
        max_turns: over.max_turns.or(base.max_turns),
        permissions: over.permissions.unwrap_or(base.permissions),
        mcp_servers: over.mcp_servers.unwrap_or(base.mcp_servers),
    }
}

pub fn load_config(cwd: &Path) -> anyhow::Result<BobConfig> {
    let mut cfg = BobConfig::default();
    if let Some(home) = dirs::home_dir() {
        if let Some(user) = read_partial(&home.join(".bob").join("settings.json"))? {
            cfg = merge(cfg, user);
        }
    }
    // Project config: prefer ./.bob/config.json; fall back to the legacy
    // ./bob.config.json so existing projects keep working.
    let project = read_partial(&cwd.join(".bob").join("config.json"))?
        .or(read_partial(&cwd.join("bob.config.json"))?);
    if let Some(project) = project {
        cfg = merge(cfg, project);
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// MCP server management — read/write ~/.bob/settings.json in place.
//
// We operate on the raw JSON (serde_json::Value) rather than BobConfig so that
// unrelated keys the user has set are preserved untouched.
// ---------------------------------------------------------------------------

fn global_settings_path() -> anyhow::Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".bob").join("settings.json"))
}

fn read_settings_value() -> anyhow::Result<serde_json::Value> {
    let path = global_settings_path()?;
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let text = std::fs::read_to_string(&path)?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    Ok(serde_json::from_str(&text)?)
}

fn write_settings_value(v: &serde_json::Value) -> anyhow::Result<()> {
    let path = global_settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(v)?)?;
    Ok(())
}

/// The configured MCP servers from the global settings file (empty if none).
pub fn list_mcp_servers() -> anyhow::Result<Vec<McpServerConfig>> {
    let settings = read_settings_value()?;
    match settings.get("mcp_servers") {
        Some(v) => Ok(serde_json::from_value(v.clone()).unwrap_or_default()),
        None => Ok(vec![]),
    }
}

/// Add (or replace, by name) an MCP server in the global settings file.
/// Returns true if an existing server with the same name was replaced.
pub fn add_mcp_server(server: McpServerConfig) -> anyhow::Result<bool> {
    let mut settings = read_settings_value()?;
    let mut servers = list_mcp_servers()?;
    let replaced = servers.iter().any(|s| s.name == server.name);
    servers.retain(|s| s.name != server.name);
    servers.push(server);
    settings["mcp_servers"] = serde_json::to_value(&servers)?;
    write_settings_value(&settings)?;
    Ok(replaced)
}

/// Remove an MCP server by name. Returns true if one was removed.
pub fn remove_mcp_server(name: &str) -> anyhow::Result<bool> {
    let mut settings = read_settings_value()?;
    let mut servers = list_mcp_servers()?;
    let before = servers.len();
    servers.retain(|s| s.name != name);
    let removed = servers.len() != before;
    settings["mcp_servers"] = serde_json::to_value(&servers)?;
    write_settings_value(&settings)?;
    Ok(removed)
}
