//! TOML config precedence: defaults, ~/.bob/config.toml, then project ancestors
//! from the repository root to cwd (nearest wins).

use super::project_context::ProjectContext;
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
    /// The command to spawn for a stdio server. Empty for HTTP servers.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// If set, this is a remote HTTP (streamable-HTTP) server reached at this URL
    /// rather than a spawned stdio subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// OAuth configuration for an HTTP server that requires authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
}

impl McpServerConfig {
    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

/// OAuth 2.0 (authorization-code + PKCE) parameters for a remote MCP server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpOAuthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    #[serde(default)]
    pub scope: String,
    /// Localhost port for the OAuth redirect callback.
    pub callback_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspServerConfig {
    /// Display name (e.g. "rust", "ts"). Namespaces the health indicator.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// File extensions this server handles, without the dot (e.g. ["rs"]).
    pub extensions: Vec<String>,
    /// Launch root, resolved relative to the declaring config's directory.
    #[serde(default = "default_root")]
    pub root: String,
}

fn default_root() -> String {
    ".".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderTimeoutsConfig {
    pub startup_secs: u64,
    pub idle_secs: u64,
}

impl Default for ProviderTimeoutsConfig {
    fn default() -> Self {
        Self {
            startup_secs: 120,
            idle_secs: 120,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BobConfig {
    /// Provider id: copilot | anthropic | openai. May also carry the model in
    /// colon form ("anthropic:claude-sonnet-4-5"); a separate `model` field wins.
    pub provider: String,
    /// Model id within the provider (e.g. "claude-sonnet-4-5", "gpt-5"). Empty =
    /// use the provider's default (or the colon form in `provider`).
    #[serde(default)]
    pub model: String,
    /// Reasoning effort label: off | low | medium | high | max. Empty/unset = off.
    /// Persisted when changed in-session so it survives a restart.
    #[serde(default)]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// TUI color theme (e.g. "dark", "light", "catppuccin"). Unknown → dark.
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_executable: Option<std::path::PathBuf>,
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub provider_timeouts: ProviderTimeoutsConfig,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(default)]
    pub lsp_servers: Vec<LspServerConfig>,
}

impl Default for BobConfig {
    fn default() -> Self {
        BobConfig {
            provider: "anthropic".to_string(),
            model: String::new(),
            reasoning: String::new(),
            system: None,
            max_turns: Some(crate::agent::agent::DEFAULT_MAX_TURNS),
            theme: None,
            bash_executable: None,
            permissions: PermissionsConfig {
                default: "ask".to_string(),
                allow_bash: [
                    "ls", "cat", "pwd", "echo", "grep", "find", "git", "bun", "node", "tsc",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
                allow: ["read_file", "list_dir", "glob", "grep", "todo_write"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                deny: vec![],
            },
            provider_timeouts: ProviderTimeoutsConfig::default(),
            mcp_servers: vec![],
            lsp_servers: vec![],
        }
    }
}

#[derive(Debug, Deserialize)]
struct PartialConfig {
    provider: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    system: Option<String>,
    max_turns: Option<u32>,
    theme: Option<String>,
    bash_executable: Option<std::path::PathBuf>,
    permissions: Option<PartialPermissions>,
    provider_timeouts: Option<PartialProviderTimeouts>,
    mcp_servers: Option<Vec<PartialMcpServer>>,
    lsp_servers: Option<Vec<PartialLspServer>>,
}

#[derive(Debug, Deserialize)]
struct PartialProviderTimeouts {
    startup_secs: Option<u64>,
    idle_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PartialPermissions {
    default: Option<String>,
    allow_bash: Option<Vec<String>>,
    allow: Option<Vec<String>>,
    deny: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct PartialMcpServer {
    name: String,
    #[serde(default)]
    disabled: bool,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<std::collections::HashMap<String, String>>,
    url: Option<String>,
    oauth: Option<McpOAuthConfig>,
}

#[derive(Debug, Deserialize)]
struct PartialLspServer {
    name: String,
    #[serde(default)]
    disabled: bool,
    command: Option<String>,
    args: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    root: Option<String>,
    #[serde(skip)]
    directory: Option<std::path::PathBuf>,
}

fn read_partial(path: &Path) -> anyhow::Result<Option<PartialConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let mut parsed: PartialConfig = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("invalid config at {}: {}", path.display(), e))?;
    if let Some(timeouts) = &parsed.provider_timeouts {
        anyhow::ensure!(
            timeouts.startup_secs != Some(0) && timeouts.idle_secs != Some(0),
            "provider timeouts must be positive at {}",
            path.display()
        );
    }
    if let Some(directory) = path.parent() {
        resolve_server_paths(&mut parsed, directory);
    }
    Ok(Some(parsed))
}

fn resolve_server_paths(config: &mut PartialConfig, directory: &Path) {
    fn command_path(command: &mut Option<String>, directory: &Path) {
        if let Some(command) = command {
            if Path::new(command).is_relative() && command.contains(std::path::is_separator) {
                *command = directory.join(&*command).to_string_lossy().into_owned();
            }
        }
    }
    fn args_paths(args: &mut Option<Vec<String>>, directory: &Path) {
        if let Some(args) = args {
            for arg in args {
                if arg
                    .strip_prefix("..")
                    .or_else(|| arg.strip_prefix('.'))
                    .is_some_and(|suffix| suffix.starts_with(std::path::is_separator))
                {
                    *arg = directory.join(&*arg).to_string_lossy().into_owned();
                }
            }
        }
    }
    for server in config.mcp_servers.iter_mut().flatten() {
        command_path(&mut server.command, directory);
        args_paths(&mut server.args, directory);
    }
    for server in config.lsp_servers.iter_mut().flatten() {
        command_path(&mut server.command, directory);
        args_paths(&mut server.args, directory);
        if let Some(root) = &mut server.root {
            if Path::new(root).is_relative() {
                *root = directory.join(&*root).to_string_lossy().into_owned();
            }
        }
        server.directory = Some(directory.to_path_buf());
    }
}

fn merge(mut base: BobConfig, over: PartialConfig) -> BobConfig {
    if let Some(permissions) = over.permissions {
        if let Some(value) = permissions.default {
            base.permissions.default = value;
        }
        if let Some(value) = permissions.allow_bash {
            base.permissions.allow_bash = value;
        }
        if let Some(value) = permissions.allow {
            base.permissions.allow = value;
        }
        if let Some(value) = permissions.deny {
            base.permissions.deny = value;
        }
    }
    if let Some(timeouts) = over.provider_timeouts {
        if let Some(value) = timeouts.startup_secs {
            base.provider_timeouts.startup_secs = value;
        }
        if let Some(value) = timeouts.idle_secs {
            base.provider_timeouts.idle_secs = value;
        }
    }
    // Named overlays preserve peers; an explicit empty list disables all inherited servers.
    if over.mcp_servers.as_ref().is_some_and(Vec::is_empty) {
        base.mcp_servers.clear();
    }
    if over.lsp_servers.as_ref().is_some_and(Vec::is_empty) {
        base.lsp_servers.clear();
    }
    for server in over.mcp_servers.into_iter().flatten() {
        if server.disabled {
            base.mcp_servers.retain(|s| s.name != server.name);
            continue;
        }
        let index = base
            .mcp_servers
            .iter()
            .position(|s| s.name == server.name)
            .unwrap_or_else(|| {
                base.mcp_servers.push(McpServerConfig {
                    name: server.name.clone(),
                    command: String::new(),
                    args: vec![],
                    env: Default::default(),
                    url: None,
                    oauth: None,
                });
                base.mcp_servers.len() - 1
            });
        let entry = &mut base.mcp_servers[index];
        if let Some(value) = server.command {
            entry.command = value;
        }
        if let Some(value) = server.args {
            entry.args = value;
        }
        if let Some(value) = server.env {
            if value.is_empty() {
                entry.env.clear();
            } else {
                entry.env.extend(value);
            }
        }
        if let Some(value) = server.url {
            entry.url = if value.is_empty() { None } else { Some(value) };
        }
        if let Some(value) = server.oauth {
            entry.oauth = Some(value);
        }
    }
    for server in over.lsp_servers.into_iter().flatten() {
        if server.disabled {
            base.lsp_servers.retain(|s| s.name != server.name);
            continue;
        }
        let index = base
            .lsp_servers
            .iter()
            .position(|s| s.name == server.name)
            .unwrap_or_else(|| {
                base.lsp_servers.push(LspServerConfig {
                    name: server.name.clone(),
                    command: String::new(),
                    args: vec![],
                    extensions: vec![],
                    root: server
                        .directory
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(default_root),
                });
                base.lsp_servers.len() - 1
            });
        let entry = &mut base.lsp_servers[index];
        if let Some(value) = server.command {
            entry.command = value;
        }
        if let Some(value) = server.args {
            entry.args = value;
        }
        if let Some(value) = server.extensions {
            entry.extensions = value;
        }
        if let Some(value) = server.root {
            entry.root = value;
        }
    }
    BobConfig {
        provider: over.provider.unwrap_or(base.provider),
        model: over.model.unwrap_or(base.model),
        reasoning: over.reasoning.unwrap_or(base.reasoning),
        system: over.system.or(base.system),
        max_turns: over.max_turns.or(base.max_turns),
        theme: over.theme.or(base.theme),
        bash_executable: over.bash_executable.or(base.bash_executable),
        permissions: base.permissions,
        provider_timeouts: base.provider_timeouts,
        mcp_servers: base.mcp_servers,
        lsp_servers: base.lsp_servers,
    }
}

pub fn load_config(cwd: &Path) -> anyhow::Result<BobConfig> {
    let mut cfg = BobConfig::default();
    if let Some(home) = dirs::home_dir() {
        if let Some(user) = read_partial(&home.join(".bob").join("config.toml"))? {
            cfg = merge(cfg, user);
        }
    }
    load_project_config(cfg, cwd)
}

pub fn load_project_config(mut cfg: BobConfig, cwd: &Path) -> anyhow::Result<BobConfig> {
    let context = ProjectContext::discover(cwd)?;
    for directory in context.directories() {
        let path = directory.join(".bob.config.toml");
        match context.bounded_path(&path) {
            Ok(path) => {
                if let Some(project) = read_partial(&path)? {
                    cfg = merge(cfg, project);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for server in &cfg.mcp_servers {
        anyhow::ensure!(
            !server.name.is_empty() && (!server.command.is_empty() || server.url.is_some()),
            "MCP server '{}' requires a name and command or URL",
            server.name
        );
    }
    for server in &cfg.lsp_servers {
        anyhow::ensure!(
            !server.name.is_empty() && !server.command.is_empty() && !server.extensions.is_empty(),
            "LSP server '{}' requires a name, command, and extensions",
            server.name
        );
    }
    Ok(cfg)
}

pub fn global_config_path() -> anyhow::Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".bob").join("config.toml"))
}

pub fn plans_dir() -> anyhow::Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".bob").join("plans"))
}

pub fn default_config_toml() -> String {
    let d = BobConfig::default();
    let quote = |v: &[String]| {
        v.iter()
            .map(|s| format!("\"{}\"", s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        r#"# bob global configuration (~/.bob/config.toml).
# A project may override any of these in ./.bob.config.toml.

# Provider: copilot | anthropic | openai. Log in first with `bob login <provider>`.
provider = "{provider}"
# Model within the provider (e.g. "claude-sonnet-4-5", "gpt-5"). Empty = default.
model = "{model}"
# Reasoning effort: off | low | medium | high | max. Updated when you change it
# in-session (/reasoning). Empty = off.
reasoning = "{reasoning}"

# Color theme: dark | light | catppuccin | catppuccin-macchiato | catppuccin-frappe
#            | catppuccin-latte | github-dark | github-light | solarized-dark
#            | solarized-light | base16-dark
theme = "dark"

# Max agent turns per prompt before it stops on its own.
max_turns = {max_turns}

# Optional: replace the built-in system prompt entirely (advanced).
# system = "You are ..."

# [provider_timeouts]
# startup_secs = 120
# idle_secs = 120

[permissions]
# What to do for a tool call not covered by the lists below: allow | deny | ask.
default = "{perm_default}"
# Shell commands auto-approved without a prompt.
allow_bash = [{allow_bash}]
# Tools auto-approved without a prompt.
allow = [{allow}]
# Tools always denied.
deny = [{deny}]

# MCP servers merge by name. Omitted fields remain inherited; arrays replace.
# Set name and disabled = true to remove an inherited MCP or LSP server.
# Relative executables and ./ or ../ arguments resolve beside this config.
# MCP servers (or add them with `bob mcp add <name> -- <command>`):
# [[mcp_servers]]
# name = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

# Language servers live in the PROJECT config (./.bob.config.toml), added with
# `bob lsp add rust --ext rs -- rust-analyzer`.
"#,
        provider = "",
        model = "",
        reasoning = "",
        max_turns = d.max_turns.unwrap_or(20),
        perm_default = d.permissions.default,
        allow_bash = quote(&d.permissions.allow_bash),
        allow = quote(&d.permissions.allow),
        deny = quote(&d.permissions.deny),
    )
}

/// Write the default config to `~/.bob/config.toml`. Returns the path written.
/// If the file exists, it is overwritten only when `force` is true.
pub fn init_global_config(force: bool) -> anyhow::Result<(std::path::PathBuf, bool)> {
    let path = global_config_path()?;
    if path.exists() && !force {
        return Ok((path, false));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, default_config_toml())?;
    Ok((path, true))
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
    let parsed: PartialConfig = toml::from_str(&text)?;
    Ok(merge(BobConfig::default(), parsed).mcp_servers)
}

fn set_mcp_servers(
    doc: &mut toml_edit::DocumentMut,
    servers: &[McpServerConfig],
) -> anyhow::Result<()> {
    use toml_edit::{value, Array, ArrayOfTables, Item, Table};
    let mut tables = ArrayOfTables::new();
    for s in servers {
        let mut t = Table::new();
        t["name"] = value(&s.name);
        if !s.command.is_empty() {
            t["command"] = value(&s.command);
        }
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
        if let Some(url) = &s.url {
            t["url"] = value(url.as_str());
        }
        if let Some(o) = &s.oauth {
            let mut ot = Table::new();
            ot["authorize_url"] = value(o.authorize_url.as_str());
            ot["token_url"] = value(o.token_url.as_str());
            ot["client_id"] = value(o.client_id.as_str());
            if !o.scope.is_empty() {
                ot["scope"] = value(o.scope.as_str());
            }
            ot["callback_port"] = value(o.callback_port as i64);
            t["oauth"] = Item::Table(ot);
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
    let mut single = toml_edit::DocumentMut::new();
    set_mcp_servers(&mut single, std::slice::from_ref(&server))?;
    let replaced = replace_named_server(
        &mut doc,
        "mcp_servers",
        &server.name,
        single["mcp_servers"]
            .as_array_of_tables()
            .unwrap()
            .get(0)
            .unwrap()
            .clone(),
    )?;
    write_config_doc(&doc)?;
    Ok(replaced)
}

fn replace_named_server(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    name: &str,
    table: toml_edit::Table,
) -> anyhow::Result<bool> {
    if doc.get(key).is_none() {
        doc[key] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let entries = doc[key]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow::anyhow!("{key} must be an array of tables"))?;
    let before = entries.len();
    entries.retain(|entry| entry.get("name").and_then(toml_edit::Item::as_str) != Some(name));
    let replaced = entries.len() != before;
    entries.push(table);
    Ok(replaced)
}

/// Preserves comments and unrelated settings; an empty model clears its override.
pub fn set_default_model(provider: &str, model: &str) -> anyhow::Result<()> {
    use toml_edit::value;
    let mut doc = read_config_doc()?;
    doc["provider"] = value(provider);
    if model.is_empty() {
        doc.as_table_mut().remove("model");
    } else {
        doc["model"] = value(model);
    }
    write_config_doc(&doc)
}

/// Empty or "off" clears the override; comments and unrelated settings survive.
pub fn set_default_reasoning(label: &str) -> anyhow::Result<()> {
    use toml_edit::value;
    let mut doc = read_config_doc()?;
    if label.is_empty() || label == "off" {
        doc.as_table_mut().remove("reasoning");
    } else {
        doc["reasoning"] = value(label);
    }
    write_config_doc(&doc)
}

/// Remove an MCP server by name. Returns true if one was removed.
pub fn remove_mcp_server(name: &str) -> anyhow::Result<bool> {
    let mut doc = read_config_doc()?;
    let Some(item) = doc.get_mut("mcp_servers") else {
        return Ok(false);
    };
    let entries = item
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow::anyhow!("mcp_servers must be an array of tables"))?;
    let before = entries.len();
    entries.retain(|entry| entry.get("name").and_then(toml_edit::Item::as_str) != Some(name));
    let removed = entries.len() != before;
    write_config_doc(&doc)?;
    Ok(removed)
}

fn project_config_path(cwd: &Path) -> anyhow::Result<std::path::PathBuf> {
    let context = ProjectContext::discover(cwd)?;
    for directory in context.directories().into_iter().rev() {
        let path = directory.join(".bob.config.toml");
        match context.bounded_path(&path) {
            Ok(path) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(context.root.join(".bob.config.toml"))
}

fn read_project_doc(cwd: &Path) -> anyhow::Result<toml_edit::DocumentMut> {
    let path = project_config_path(cwd)?;
    if !path.exists() {
        return Ok(toml_edit::DocumentMut::new());
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(text.parse::<toml_edit::DocumentMut>()?)
}

fn write_project_doc(cwd: &Path, doc: &toml_edit::DocumentMut) -> anyhow::Result<()> {
    std::fs::write(project_config_path(cwd)?, doc.to_string())?;
    Ok(())
}

/// The configured LSP servers from the project config file (empty if none).
pub fn list_lsp_servers(cwd: &Path) -> anyhow::Result<Vec<LspServerConfig>> {
    Ok(load_project_config(BobConfig::default(), cwd)?.lsp_servers)
}

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
        let mut arr = Array::new();
        for a in &s.args {
            arr.push(a.as_str());
        }
        t["args"] = value(arr);
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
    let replaced = list_lsp_servers(cwd)?.iter().any(|s| s.name == server.name);
    let mut single = toml_edit::DocumentMut::new();
    set_lsp_servers(&mut single, std::slice::from_ref(&server))?;
    replace_named_server(
        &mut doc,
        "lsp_servers",
        &server.name,
        single["lsp_servers"]
            .as_array_of_tables()
            .unwrap()
            .get(0)
            .unwrap()
            .clone(),
    )?;
    write_project_doc(cwd, &doc)?;
    Ok(replaced)
}

/// Remove an LSP server by name. Returns true if one was removed.
pub fn remove_lsp_server(cwd: &Path, name: &str) -> anyhow::Result<bool> {
    if !list_lsp_servers(cwd)?
        .iter()
        .any(|server| server.name == name)
    {
        return Ok(false);
    }
    let mut doc = read_project_doc(cwd)?;
    let mut disabled = toml_edit::Table::new();
    disabled["name"] = toml_edit::value(name);
    disabled["disabled"] = toml_edit::value(true);
    replace_named_server(&mut doc, "lsp_servers", name, disabled)?;
    write_project_doc(cwd, &doc)?;
    Ok(true)
}

pub fn set_theme_in_project(cwd: &Path, theme: &str) -> anyhow::Result<()> {
    let mut doc = read_project_doc(cwd)?;
    doc["theme"] = toml_edit::value(theme);
    write_project_doc(cwd, &doc)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn named_server_and_permission_overlays_distinguish_omitted_empty_and_disabled() {
        let base: BobConfig = toml::from_str(
            r#"
            provider = "anthropic"
            [permissions]
            default = "ask"
            allow = ["read_file"]
            allow_bash = ["git"]
            deny = ["bash"]
            [[mcp_servers]]
            name = "one"
            command = "node"
            args = ["server.js"]
            env = {KEEP = "yes", CHANGE = "old"}
            [[mcp_servers]]
            name = "two"
            command = "other"
            [[lsp_servers]]
            name = "rust"
            command = "rust-analyzer"
            args = ["--old"]
            extensions = ["rs"]
            root = "/repo/rust"
        "#,
        )
        .unwrap();
        let mut overlaid = base.clone();
        overlaid.permissions.allow = vec!["grep".into()];
        overlaid.mcp_servers[0]
            .env
            .insert("CHANGE".into(), "new".into());
        overlaid.lsp_servers[0].args = vec!["--verbose".into()];

        let mut empty_fields = base.clone();
        empty_fields.permissions.allow.clear();
        empty_fields.permissions.allow_bash.clear();
        empty_fields.permissions.deny.clear();
        empty_fields.mcp_servers[0].args.clear();
        empty_fields.mcp_servers[0].env.clear();
        empty_fields.lsp_servers[0].args.clear();
        empty_fields.lsp_servers[0].extensions.clear();

        let mut no_servers = base.clone();
        no_servers.mcp_servers.clear();
        no_servers.lsp_servers.clear();

        let mut disabled = base.clone();
        disabled.mcp_servers.remove(0);
        disabled.lsp_servers.clear();

        for (case, text, expected) in [
            ("omitted lists", "", base.clone()),
            (
                "named partial overlays preserve peers and omitted fields",
                r#"
                [permissions]
                allow = ["grep"]
                [[mcp_servers]]
                name = "one"
                env = {CHANGE = "new"}
                [[lsp_servers]]
                name = "rust"
                args = ["--verbose"]
                "#,
                overlaid,
            ),
            (
                "explicit empty fields replace inherited values",
                r#"
                [permissions]
                allow = []
                allow_bash = []
                deny = []
                [[mcp_servers]]
                name = "one"
                args = []
                env = {}
                [[lsp_servers]]
                name = "rust"
                args = []
                extensions = []
                "#,
                empty_fields,
            ),
            (
                "empty lists disable all integrations",
                "mcp_servers = []\nlsp_servers = []",
                no_servers,
            ),
            (
                "disabled names remove only matching integrations",
                r#"
                [[mcp_servers]]
                name = "one"
                disabled = true
                [[lsp_servers]]
                name = "rust"
                disabled = true
                "#,
                disabled,
            ),
            (
                "disabled unknown names leave inherited integrations intact",
                r#"
                [[mcp_servers]]
                name = "missing"
                disabled = true
                [[lsp_servers]]
                name = "missing"
                disabled = true
                "#,
                base.clone(),
            ),
        ] {
            let actual = merge(base.clone(), toml::from_str(text).unwrap());
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{case}"
            );
        }
    }

    #[test]
    fn nested_launch_merges_nearest_and_resolves_declaring_relative_paths() {
        let root = TestDir::new();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        std::fs::write(
            root.join(".bob.config.toml"),
            r#"
            model = "root"
            [[lsp_servers]]
            name = "rust"
            command = "./bin/server"
            args = ["./server.config"]
            extensions = ["rs"]
            root = "src"
            [[mcp_servers]]
            name = "mcp"
            command = "node"
            args = ["./server.js"]
        "#,
        )
        .unwrap();
        std::fs::write(
            root.join("nested/.bob.config.toml"),
            r#"
            model = "nearest"
            [[lsp_servers]]
            name = "rust"
            command = "rust-analyzer"
            extensions = ["rs", "toml"]
        "#,
        )
        .unwrap();
        let cwd = root.join("nested/deep");
        let config = load_project_config(BobConfig::default(), &cwd).unwrap();
        assert_eq!(config.model, "nearest");
        assert_eq!(config.lsp_servers.len(), 1);
        assert_eq!(config.lsp_servers[0].name, "rust");
        assert_eq!(config.lsp_servers[0].command, "rust-analyzer");
        assert_eq!(config.lsp_servers[0].extensions, ["rs", "toml"]);
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "mcp");
        assert_eq!(config.mcp_servers[0].command, "node");
        assert_eq!(
            config.lsp_servers[0].root,
            root.join("src").to_string_lossy()
        );
        assert_eq!(
            config.lsp_servers[0].args[0],
            root.join("./server.config").to_string_lossy()
        );
        assert_eq!(
            config.mcp_servers[0].args[0],
            root.join("./server.js").to_string_lossy()
        );
        assert_eq!(
            project_config_path(&cwd).unwrap(),
            root.join("nested/.bob.config.toml")
        );
        assert!(remove_lsp_server(&cwd, "rust").unwrap());
        assert!(load_project_config(BobConfig::default(), &cwd)
            .unwrap()
            .lsp_servers
            .is_empty());
        assert_eq!(
            load_project_config(BobConfig::default(), &root)
                .unwrap()
                .lsp_servers
                .len(),
            1
        );
    }

    #[test]
    fn timeout_fields_merge_individually_and_zero_is_rejected() {
        let defaults = BobConfig::default();
        let config = merge(
            defaults,
            toml::from_str("[provider_timeouts]\nstartup_secs = 5").unwrap(),
        );
        assert_eq!(config.provider_timeouts.startup_secs, 5);
        assert_eq!(config.provider_timeouts.idle_secs, 120);
        let config = merge(
            config,
            toml::from_str("[provider_timeouts]\nidle_secs = 9").unwrap(),
        );
        assert_eq!(config.provider_timeouts.startup_secs, 5);
        assert_eq!(config.provider_timeouts.idle_secs, 9);
        let root = TestDir::new();
        std::fs::create_dir(root.join(".git")).unwrap();
        let path = root.join(".bob.config.toml");
        for field in ["startup_secs", "idle_secs"] {
            std::fs::write(&path, format!("[provider_timeouts]\n{field} = 0")).unwrap();
            let error = load_project_config(BobConfig::default(), &root).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("provider timeouts must be positive at {}", path.display())
            );
            std::fs::write(&path, format!("[provider_timeouts]\n{field} = 1")).unwrap();
            let config = load_project_config(BobConfig::default(), &root).unwrap();
            assert_eq!(
                (
                    config.provider_timeouts.startup_secs,
                    config.provider_timeouts.idle_secs
                ),
                if field == "startup_secs" {
                    (1, 120)
                } else {
                    (120, 1)
                }
            );
        }
    }

    #[test]
    fn named_updates_preserve_other_partial_entries_and_tombstones() {
        let mut doc: toml_edit::DocumentMut = "[[lsp_servers]]\nname = 'removed'\ndisabled = true\n[[lsp_servers]]\nname = 'partial'\nargs = ['x']\n".parse().unwrap();
        let mut table = toml_edit::Table::new();
        table["name"] = toml_edit::value("new");
        table["command"] = toml_edit::value("server");
        assert!(!replace_named_server(&mut doc, "lsp_servers", "new", table).unwrap());
        let parsed: PartialConfig = toml::from_str(&doc.to_string()).unwrap();
        let entries = parsed.lsp_servers.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "removed");
        assert!(entries[0].disabled);
        assert!(entries[0].command.is_none());
        assert_eq!(entries[1].name, "partial");
        assert_eq!(entries[1].args.as_deref(), Some(&["x".to_string()][..]));
        assert!(entries[1].command.is_none());
        assert_eq!(entries[2].name, "new");
        assert_eq!(entries[2].command.as_deref(), Some("server"));
    }

    #[cfg(unix)]
    #[test]
    fn project_config_symlinks_cannot_escape_boundary() {
        let base = TestDir::new();
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(base.join("outside.toml"), "model = 'outside'").unwrap();
        std::os::unix::fs::symlink(base.join("outside.toml"), root.join(".bob.config.toml"))
            .unwrap();
        for error in [
            load_project_config(BobConfig::default(), &root).unwrap_err(),
            project_config_path(&root).unwrap_err(),
        ] {
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn parsed_config_overrides_defaults_and_preserves_omitted_fields() {
        let default = BobConfig::default();
        assert_eq!(
            default.max_turns,
            Some(crate::agent::agent::DEFAULT_MAX_TURNS)
        );
        let text = r#"
            provider = "anthropic:claude-sonnet"
            max_turns = 12
            reasoning = "high"
            [permissions]
            default = "allow"
            allow_bash = ["ls", "git"]
            allow = ["read_file"]
            deny = []
        "#;
        let config = merge(default, toml::from_str(text).unwrap());
        assert_eq!(config.provider, "anthropic:claude-sonnet");
        assert_eq!(config.max_turns, Some(12));
        assert_eq!(config.reasoning, "high");
        assert_eq!(config.permissions.default, "allow");
        assert_eq!(config.permissions.allow_bash, ["ls", "git"]);
        assert_eq!(config.permissions.allow, ["read_file"]);
        assert!(config.permissions.deny.is_empty());

        let inherited = merge(config.clone(), toml::from_str("").unwrap());
        assert_eq!(
            serde_json::to_value(inherited).unwrap(),
            serde_json::json!({
                "provider": "anthropic:claude-sonnet",
                "model": "",
                "reasoning": "high",
                "max_turns": 12,
                "theme": null,
                "permissions": {
                    "default": "allow",
                    "allow_bash": ["ls", "git"],
                    "allow": ["read_file"],
                    "deny": []
                },
                "provider_timeouts": {"startup_secs": 120, "idle_secs": 120},
                "mcp_servers": [],
                "lsp_servers": []
            })
        );
        let cleared = merge(config, toml::from_str("reasoning = ''").unwrap());
        assert_eq!(cleared.reasoning, "");
        assert_eq!(cleared.max_turns, Some(12));
        assert_eq!(cleared.provider, "anthropic:claude-sonnet");
    }

    #[test]
    fn default_template_parses_and_carries_reasoning() {
        let toml_str = default_config_toml();
        let p: PartialConfig = toml::from_str(&toml_str).expect("default template is valid TOML");
        assert_eq!(p.reasoning.as_deref(), Some(""));
        assert_eq!(p.model.as_deref(), Some(""));
        assert_eq!(p.max_turns, Some(200));
        assert_eq!(p.theme.as_deref(), Some("dark"));
        assert!(p.system.is_none());
        let permissions = p.permissions.as_ref().unwrap();
        assert_eq!(permissions.default.as_deref(), Some("ask"));
        assert_eq!(
            permissions.allow.as_ref().unwrap(),
            &["read_file", "list_dir", "glob", "grep", "todo_write"]
        );
        assert_eq!(
            permissions.allow_bash.as_ref().unwrap(),
            &["ls", "cat", "pwd", "echo", "grep", "find", "git", "bun", "node", "tsc"]
        );
        assert!(permissions.deny.as_ref().unwrap().is_empty());
        let cfg = merge(BobConfig::default(), p);
        assert_eq!(cfg.reasoning, "");
    }

    #[test]
    fn set_mcp_servers_preserves_comments() {
        let mut doc = "# keep me\nprovider = \"anthropic\"\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let servers = vec![McpServerConfig {
            name: "fs".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            env: std::collections::HashMap::new(),
            url: None,
            oauth: None,
        }];
        set_mcp_servers(&mut doc, &servers).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# keep me"), "comment must survive");
        assert!(out.contains("provider = \"anthropic\""));
        assert!(out.contains("[[mcp_servers]]"));
        assert!(out.contains("name = \"fs\""));

        #[derive(Deserialize, Default)]
        struct OnlyMcp {
            #[serde(default)]
            mcp_servers: Vec<McpServerConfig>,
        }
        let parsed: OnlyMcp = toml::from_str(&out).unwrap();
        assert_eq!(parsed.mcp_servers.len(), 1);
        assert_eq!(parsed.mcp_servers[0].name, "fs");
        assert_eq!(parsed.mcp_servers[0].command, "npx");
        assert_eq!(parsed.mcp_servers[0].args, ["-y", "server"]);
        assert!(parsed.mcp_servers[0].env.is_empty());
        assert!(!parsed.mcp_servers[0].is_http());
        assert!(parsed.mcp_servers[0].oauth.is_none());
    }

    #[test]
    fn set_mcp_servers_roundtrips_http_with_oauth() {
        let mut doc = "provider = \"anthropic\"\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let servers = vec![McpServerConfig {
            name: "github".into(),
            command: String::new(),
            args: vec![],
            env: std::collections::HashMap::new(),
            url: Some("https://api.githubcopilot.com/mcp/".into()),
            oauth: Some(McpOAuthConfig {
                authorize_url: "https://example.com/authorize".into(),
                token_url: "https://example.com/token".into(),
                client_id: "abc123".into(),
                scope: "repo".into(),
                callback_port: 54546,
            }),
        }];
        set_mcp_servers(&mut doc, &servers).unwrap();
        let out = doc.to_string();
        assert!(!out.contains("command ="), "http server has no command");
        assert!(out.contains("url = \"https://api.githubcopilot.com/mcp/\""));
        assert!(out.contains("client_id = \"abc123\""));

        #[derive(Deserialize, Default)]
        struct OnlyMcp {
            #[serde(default)]
            mcp_servers: Vec<McpServerConfig>,
        }
        let parsed: OnlyMcp = toml::from_str(&out).unwrap();
        assert_eq!(parsed.mcp_servers.len(), 1);
        let s = &parsed.mcp_servers[0];
        assert!(s.is_http());
        assert_eq!(s.command, "");
        let o = s.oauth.as_ref().unwrap();
        assert_eq!(o.client_id, "abc123");
        assert_eq!(o.callback_port, 54546);
        assert_eq!(o.authorize_url, "https://example.com/authorize");
        assert_eq!(o.token_url, "https://example.com/token");
        assert_eq!(o.scope, "repo");
        assert_eq!(s.url.as_deref(), Some("https://api.githubcopilot.com/mcp/"));
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
        assert_eq!(parsed.lsp_servers[0].name, "rust");
        assert_eq!(parsed.lsp_servers[0].extensions, ["rs"]);
        assert!(parsed.lsp_servers[0].args.is_empty());
    }

    #[test]
    fn empty_project_config_uses_complete_safe_defaults() {
        let root = TestDir::new();
        std::fs::create_dir(root.join(".git")).unwrap();
        let config = load_project_config(BobConfig::default(), &root).unwrap();
        assert_eq!(
            serde_json::to_value(config).unwrap(),
            serde_json::json!({
                "provider": "anthropic",
                "model": "",
                "reasoning": "",
                "max_turns": 200,
                "theme": null,
                "permissions": {
                    "default": "ask",
                    "allow_bash": ["ls", "cat", "pwd", "echo", "grep", "find", "git", "bun", "node", "tsc"],
                    "allow": ["read_file", "list_dir", "glob", "grep", "todo_write"],
                    "deny": []
                },
                "provider_timeouts": {"startup_secs": 120, "idle_secs": 120},
                "mcp_servers": [],
                "lsp_servers": []
            })
        );
    }

    #[test]
    fn read_partial_distinguishes_missing_invalid_and_unreadable_config() {
        let root = TestDir::new();
        let path = root.join("config.toml");
        assert!(read_partial(&path).unwrap().is_none());
        for text in [
            "provider = [",
            "max_turns = -1",
            "max_turns = 4294967296",
            "[provider_timeouts]\nstartup_secs = -1",
            "[provider_timeouts]\nidle_secs = '120'",
            "[[mcp_servers]]\ncommand = 'node'",
        ] {
            std::fs::write(&path, text).unwrap();
            let error = read_partial(&path).unwrap_err().to_string();
            assert!(
                error.starts_with(&format!("invalid config at {}:", path.display())),
                "{text}: {error}"
            );
        }
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            read_partial(&path)
                .unwrap_err()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn validation_rejects_incomplete_servers_after_overlays() {
        let root = TestDir::new();
        std::fs::create_dir(root.join(".git")).unwrap();
        for (text, expected) in [
            (
                "[[mcp_servers]]\nname = ''\ncommand = 'node'",
                "MCP server '' requires a name and command or URL",
            ),
            (
                "[[mcp_servers]]\nname = 'missing'",
                "MCP server 'missing' requires a name and command or URL",
            ),
            (
                "[[mcp_servers]]\nname = 'empty'\nurl = ''",
                "MCP server 'empty' requires a name and command or URL",
            ),
            (
                "[[lsp_servers]]\nname = ''\ncommand = 'server'\nextensions = ['rs']",
                "LSP server '' requires a name, command, and extensions",
            ),
            (
                "[[lsp_servers]]\nname = 'missing'\nextensions = ['rs']",
                "LSP server 'missing' requires a name, command, and extensions",
            ),
            (
                "[[lsp_servers]]\nname = 'empty'\ncommand = 'server'\nextensions = []",
                "LSP server 'empty' requires a name, command, and extensions",
            ),
        ] {
            std::fs::write(root.join(".bob.config.toml"), text).unwrap();
            let error = load_project_config(BobConfig::default(), &root).unwrap_err();
            assert_eq!(error.to_string(), expected, "{text}");
        }
        std::fs::write(root.join(".bob.config.toml"), "[[mcp_servers]]\nname = 'remote'\nurl = 'https://example.com/mcp'\n[[lsp_servers]]\nname = 'disabled'\ndisabled = true").unwrap();
        let config = load_project_config(BobConfig::default(), &root).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert!(config.mcp_servers[0].is_http());
        assert_eq!(
            config.mcp_servers[0].url.as_deref(),
            Some("https://example.com/mcp")
        );
        assert!(config.lsp_servers.is_empty());
    }

    #[test]
    fn server_paths_resolve_only_path_arguments_beside_declaring_config() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("config-layer");
        let absolute = directory.join("absolute").to_string_lossy().into_owned();
        let mut partial: PartialConfig = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "local"
            command = "./bin/server"
            args = ["./script.js", "../shared.js", "plain.js", "--config=./config.json", "--config", "config/settings.json", '.\script.js', '..\shared.js', '--config=.\config.json']
            [[mcp_servers]]
            name = "native"
            command = '.\bin\server'
            [[lsp_servers]]
            name = "rust"
            command = "rust-analyzer"
            extensions = ["rs"]
            [[lsp_servers]]
            name = "nested"
            command = "server"
            extensions = ["ts"]
            root = "src"
        "#,
        )
        .unwrap();
        partial.lsp_servers.as_mut().unwrap()[1].command = Some(absolute.clone());
        partial.mcp_servers.as_mut().unwrap()[0]
            .args
            .as_mut()
            .unwrap()
            .push(absolute.clone());
        resolve_server_paths(&mut partial, &directory);
        let config = merge(BobConfig::default(), partial);
        assert_eq!(
            config.mcp_servers[0].command,
            directory.join("./bin/server").to_string_lossy()
        );
        let native_path = |path: &str| {
            if cfg!(windows) {
                directory.join(path).to_string_lossy().into_owned()
            } else {
                path.to_string()
            }
        };
        assert_eq!(config.mcp_servers[1].command, native_path(r".\bin\server"));
        assert_eq!(
            config.mcp_servers[0].args,
            [
                directory.join("./script.js").to_string_lossy().into_owned(),
                directory
                    .join("../shared.js")
                    .to_string_lossy()
                    .into_owned(),
                "plain.js".into(),
                "--config=./config.json".into(),
                "--config".into(),
                "config/settings.json".into(),
                native_path(r".\script.js"),
                native_path(r"..\shared.js"),
                r"--config=.\config.json".into(),
                absolute.clone(),
            ]
        );
        assert_eq!(config.lsp_servers[0].command, "rust-analyzer");
        assert_eq!(config.lsp_servers[0].root, directory.to_string_lossy());
        assert_eq!(config.lsp_servers[1].command, absolute);
        assert_eq!(
            config.lsp_servers[1].root,
            directory.join("src").to_string_lossy()
        );
    }

    #[test]
    fn named_replacement_removes_duplicates_and_rejects_wrong_table_shape() {
        let mut doc: toml_edit::DocumentMut = "# keep\nprovider = 'copilot'\n[[mcp_servers]]\nname = 'same'\ncommand = 'old'\n[[mcp_servers]]\nname = 'peer'\ncommand = 'peer-command'\n[[mcp_servers]]\nname = 'same'\ncommand = 'duplicate'\n".parse().unwrap();
        let mut replacement = toml_edit::Table::new();
        replacement["name"] = toml_edit::value("same");
        replacement["command"] = toml_edit::value("new");
        assert!(
            replace_named_server(&mut doc, "mcp_servers", "same", replacement.clone()).unwrap()
        );
        assert!(doc.to_string().starts_with("# keep\n"));
        let actual: toml::Value = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::json!({
                "provider": "copilot",
                "mcp_servers": [
                    {"name": "peer", "command": "peer-command"},
                    {"name": "same", "command": "new"}
                ]
            })
        );
        let mut invalid: toml_edit::DocumentMut = "mcp_servers = 'not a list'".parse().unwrap();
        let before = invalid.to_string();
        let error =
            replace_named_server(&mut invalid, "mcp_servers", "same", replacement).unwrap_err();
        assert_eq!(error.to_string(), "mcp_servers must be an array of tables");
        assert_eq!(invalid.to_string(), before);
    }

    #[test]
    fn bash_executable_inherits_global_and_nearest_project_override() {
        let root = TestDir::new();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        let global_path = root.join("global Git/bin/bash.exe");
        let project_path = root.join("project Git 終/bin/bash.exe");
        let global = merge(
            BobConfig::default(),
            toml::from_str(&format!("bash_executable = '{}'", global_path.display())).unwrap(),
        );
        let cwd = root.join("nested/deep");
        assert_eq!(
            load_project_config(global.clone(), &cwd)
                .unwrap()
                .bash_executable,
            Some(global_path),
        );
        std::fs::write(
            root.join(".bob.config.toml"),
            format!("bash_executable = '{}'", project_path.display()),
        )
        .unwrap();
        std::fs::write(root.join("nested/.bob.config.toml"), "model = 'nearest'").unwrap();
        let config = load_project_config(global.clone(), &cwd).unwrap();
        assert_eq!(config.bash_executable, Some(project_path));
        assert_eq!(config.model, "nearest");
        std::fs::write(
            root.join("nested/.bob.config.toml"),
            "bash_executable = 'relative/bash'",
        )
        .unwrap();
        let config = load_project_config(global, &cwd).unwrap();
        let shell =
            crate::core::shell::CommandShell::resolve(&cwd, config.bash_executable.as_deref());
        assert!(shell.executable().unwrap_err().contains("must be absolute"));
    }

    #[test]
    fn scalar_overlays_replace_present_values_and_preserve_omitted_values() {
        let base = BobConfig {
            provider: "openai".into(),
            model: "old-model".into(),
            reasoning: "low".into(),
            system: Some("old prompt".into()),
            max_turns: Some(8),
            theme: Some("light".into()),
            ..BobConfig::default()
        };
        let config = merge(
            base,
            toml::from_str(
                r#"
            model = "new-model"
            reasoning = "high"
            system = "new prompt"
            max_turns = 12
            theme = "dark"
        "#,
            )
            .unwrap(),
        );
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "new-model");
        assert_eq!(config.reasoning, "high");
        assert_eq!(config.system.as_deref(), Some("new prompt"));
        assert_eq!(config.max_turns, Some(12));
        assert_eq!(config.theme.as_deref(), Some("dark"));
        let cleared = merge(
            config,
            toml::from_str("model = ''\nreasoning = ''").unwrap(),
        );
        assert_eq!(cleared.model, "");
        assert_eq!(cleared.reasoning, "");
        assert_eq!(cleared.provider, "openai");
        assert_eq!(cleared.system.as_deref(), Some("new prompt"));
        assert_eq!(cleared.max_turns, Some(12));
        assert_eq!(cleared.theme.as_deref(), Some("dark"));
    }
}
