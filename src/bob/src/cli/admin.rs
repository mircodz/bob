use anyhow::Context;
use bob_core::core::config::load_config;
use clap::Subcommand;
use std::io::Write;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Log in to a provider (interactive).
    Login {
        /// copilot | anthropic | openai
        provider: String,
    },
    /// Forget a provider's stored credentials.
    Logout {
        /// copilot | anthropic | openai
        provider: String,
    },
    /// Show which providers you're authenticated with.
    Auth,
    /// Show the effective configuration, or scaffold a default one (`config init`).
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Manage MCP servers (stored in ~/.bob/config.toml).
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Manage language servers (stored in ./.bob.config.toml for this project).
    Lsp {
        #[command(subcommand)]
        action: LspAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Write a commented default config to ~/.bob/config.toml.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// Add an MCP server. For stdio, put the command after `--`. For a remote
    /// HTTP server, pass `--url` instead.
    ///
    /// e.g.  bob mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path
    ///       bob mcp add github --url https://api.githubcopilot.com/mcp/
    Add {
        /// A name for the server (namespaces its tools as <name>.<tool>).
        name: String,
        /// Remote HTTP server URL. If set, this is an HTTP (not stdio) server.
        #[arg(long)]
        url: Option<String>,
        /// Repeatable env var, KEY=VALUE (stdio only).
        #[arg(short, long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// The command and its args (everything after `--`) for a stdio server.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Authorize a remote HTTP MCP server. Uses OAuth (opens a browser) by
    /// default, or stores a static token if you pass --token.
    Login {
        name: String,
        /// Use this bearer token directly (e.g. a GitHub Personal Access Token)
        /// instead of the OAuth browser flow.
        #[arg(long)]
        token: Option<String>,
    },
    /// List configured MCP servers.
    List,
    /// Remove an MCP server by name.
    Remove { name: String },
}

#[derive(Debug, Subcommand)]
pub enum LspAction {
    /// Add a language server for this project. Put the command after `--`.
    ///
    /// e.g.  bob lsp add rust --ext rs -- rust-analyzer
    ///       bob lsp add ts --ext ts,tsx --root web -- typescript-language-server --stdio
    Add {
        /// A name for the server (labels it in the health indicator).
        name: String,
        /// Comma-separated file extensions the server handles, without dots.
        #[arg(short = 'e', long = "ext", value_name = "rs,ts", required = true)]
        ext: String,
        /// Project root the server runs in, relative to the repo (default ".").
        #[arg(short = 'r', long = "root", default_value = ".")]
        root: String,
        /// The command and its args (everything after `--`).
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List configured language servers for this project.
    List,
    /// Remove a language server by name.
    Remove { name: String },
}

pub async fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Login { provider } => run_login(&provider).await,
        Command::Logout { provider } => run_logout(&provider).await,
        Command::Auth => show_auth(),
        Command::Config { action } => run_config(action),
        Command::Mcp { action } => run_mcp(action).await,
        Command::Lsp { action } => run_lsp(action),
    }
}

async fn run_login(which: &str) -> anyhow::Result<()> {
    match which {
        "copilot" | "github" => {
            let device = bob_core::auth::copilot::begin_login().await?;
            println!("\nTo authorize bob with GitHub Copilot:");
            println!("  1. open  \x1b[36m{}\x1b[0m", device.verification_uri);
            println!("  2. enter code  \x1b[1m{}\x1b[0m\n", device.user_code);
            wait_dots();
            bob_core::auth::copilot::finish_login(&device, dot).await?;
            done("Copilot", "copilot")
        }
        "anthropic" | "claude" => {
            let handle = bob_core::auth::anthropic::begin_login();
            println!("\nTo authorize bob with your Claude (Pro/Max) subscription:");
            println!(
                "  open this URL in your browser:\n  \x1b[36m{}\x1b[0m\n",
                handle.url
            );
            println!("waiting for you to approve in the browser…");
            bob_core::auth::anthropic::finish_login(handle).await?;
            done("Anthropic", "anthropic")
        }
        "openai" | "chatgpt" => {
            let device = bob_core::auth::openai::begin_login().await?;
            println!("\nTo authorize bob with your ChatGPT (Plus/Pro) subscription:");
            println!("  1. open  \x1b[36m{}\x1b[0m", device.verification_uri);
            println!("  2. enter code  \x1b[1m{}\x1b[0m\n", device.user_code);
            wait_dots();
            bob_core::auth::openai::finish_login(&device, dot).await?;
            done("OpenAI", "openai")
        }
        other => anyhow::bail!(
            "unknown provider '{}'. known: copilot, anthropic, openai",
            other
        ),
    }
}

async fn run_logout(which: &str) -> anyhow::Result<()> {
    let id = match which {
        "github" => "copilot",
        "claude" => "anthropic",
        "chatgpt" => "openai",
        other => other,
    };
    if bob_core::auth::AuthStore::remove_credential(id).await? {
        println!("logged out of {}", id);
    } else {
        println!("not logged in to {}", id);
    }
    Ok(())
}

fn show_auth() -> anyhow::Result<()> {
    let store = bob_core::auth::AuthStore::load();
    let logged = store.logged_in();
    println!("authentication:");
    for prov in ["copilot", "anthropic", "openai"] {
        let status = if logged.iter().any(|p| p == prov) {
            "\x1b[32m✓ logged in\x1b[0m"
        } else {
            "\x1b[90m— not logged in\x1b[0m"
        };
        println!("  {:<10} {}", prov, status);
    }
    for (env, prov) in [
        ("ANTHROPIC_API_KEY", "anthropic"),
        ("OPENAI_API_KEY", "openai"),
    ] {
        if std::env::var(env).is_ok() {
            println!("  \x1b[90m{} is set (api key for {})\x1b[0m", env, prov);
        }
    }
    Ok(())
}

fn run_config(action: Option<ConfigAction>) -> anyhow::Result<()> {
    match action {
        None => show_config(),
        Some(ConfigAction::Init { force }) => {
            let (path, written) = bob_core::core::config::init_global_config(force)?;
            if written {
                println!("\x1b[32mwrote\x1b[0m default config to {}", path.display());
                println!("edit it, or override per-project in ./.bob.config.toml");
            } else {
                println!(
                    "config already exists at {} (use --force to overwrite)",
                    path.display()
                );
            }
            Ok(())
        }
    }
}

fn show_config() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = load_config(&cwd)?;
    println!("effective config:");
    println!("  provider:    {}", config.provider);
    println!(
        "  model:       {}",
        if config.model.is_empty() {
            "(provider default)"
        } else {
            &config.model
        }
    );
    println!(
        "  system:      {}",
        if config.system.is_some() {
            "(custom)"
        } else {
            "(default bob prompt)"
        }
    );
    println!("  max_turns:   {}", config.max_turns.unwrap_or(20));
    println!(
        "  theme:       {}",
        config.theme.as_deref().unwrap_or("dark")
    );
    println!("  permissions: default={}", config.permissions.default);
    println!("  mcp_servers: {}", config.mcp_servers.len());
    println!("  lsp_servers: {}", config.lsp_servers.len());
    match &config.shell {
        Some(shell) => println!("  shell:       {} ({})", shell.dialect.name(), shell.executable.display()),
        None => println!("  shell:       automatic (native PowerShell 7, then Git Bash on Windows; Bash elsewhere)"),
    }
    println!("\nsources (later overrides earlier):");
    println!("  1. built-in defaults");
    println!("  2. ~/.bob/config.toml");
    println!("  3. ./.bob.config.toml");
    Ok(())
}

async fn run_mcp(action: McpAction) -> anyhow::Result<()> {
    use bob_core::core::config::{
        add_mcp_server, list_mcp_servers, remove_mcp_server, McpServerConfig,
    };
    match action {
        McpAction::Add {
            name,
            url,
            env,
            command,
        } => {
            if let Some(url) = url {
                let replaced = add_mcp_server(McpServerConfig {
                    name: name.clone(),
                    command: String::new(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    url: Some(url),
                    oauth: None,
                })?;
                println!(
                    "\x1b[32m{}\x1b[0m HTTP MCP server '{}' in ~/.bob/config.toml",
                    if replaced { "updated" } else { "added" },
                    name
                );
                println!(
                    "  authorize it with:  \x1b[36mbob mcp login {}\x1b[0m",
                    name
                );
                return Ok(());
            }
            let mut parts = command.into_iter();
            let cmd = parts.next().ok_or_else(|| {
                anyhow::anyhow!(
                    "no command given (put it after `--`, or pass --url for an HTTP server)"
                )
            })?;
            let args: Vec<String> = parts.collect();
            let mut env_map = std::collections::HashMap::new();
            for pair in env {
                match pair.split_once('=') {
                    Some((k, v)) => {
                        env_map.insert(k.to_string(), v.to_string());
                    }
                    None => anyhow::bail!("bad --env '{}', expected KEY=VALUE", pair),
                }
            }
            let replaced = add_mcp_server(McpServerConfig {
                name: name.clone(),
                command: cmd,
                args,
                env: env_map,
                url: None,
                oauth: None,
            })?;
            println!(
                "\x1b[32m{}\x1b[0m MCP server '{}' in ~/.bob/config.toml",
                if replaced { "updated" } else { "added" },
                name
            );
            Ok(())
        }
        McpAction::Login { name, token } => {
            let servers = list_mcp_servers()?;
            let server = servers
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| anyhow::anyhow!("no MCP server named '{}'", name))?;
            if server.url.is_none() {
                anyhow::bail!(
                    "'{}' is a stdio server; login is only for HTTP servers",
                    name
                );
            }

            if let Some(token) = token {
                bob_core::auth::mcp::store_static_token(&name, &token).await?;
                println!("\x1b[32mstored token\x1b[0m for MCP server '{}'", name);
                return Ok(());
            }

            let url = server.url.clone().unwrap();
            let oauth = match &server.oauth {
                Some(o) => o.clone(),
                None => {
                    println!("discovering OAuth configuration for '{}'…", name);
                    let discovered = bob_core::auth::mcp::discover(&name, &url).await?;
                    let mut updated = server.clone();
                    updated.oauth = Some(discovered.clone());
                    add_mcp_server(updated)?;
                    discovered
                }
            };

            let handle = bob_core::auth::mcp::begin_login(&name, &oauth);
            println!("\nTo authorize bob with the '{}' MCP server:", name);
            println!("  open  \x1b[36m{}\x1b[0m", handle.url);
            println!("waiting for you to approve in the browser…");
            bob_core::auth::mcp::finish_login(handle).await?;
            println!("\x1b[32mauthorized\x1b[0m MCP server '{}'", name);
            Ok(())
        }
        McpAction::List => {
            let servers = list_mcp_servers()?;
            if servers.is_empty() {
                println!("no MCP servers configured. Add one with:");
                println!("  \x1b[36mbob mcp add <name> -- <command> [args...]\x1b[0m");
                return Ok(());
            }
            println!("MCP servers (~/.bob/config.toml):");
            for s in servers {
                if let Some(url) = &s.url {
                    let auth = if s.oauth.is_some() { " (oauth)" } else { "" };
                    println!(
                        "  \x1b[1m{}\x1b[0m  \x1b[90mhttp {}{}\x1b[0m",
                        s.name, url, auth
                    );
                } else {
                    let args = if s.args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", s.args.join(" "))
                    };
                    println!(
                        "  \x1b[1m{}\x1b[0m  \x1b[90m{}{}\x1b[0m",
                        s.name, s.command, args
                    );
                }
            }
            Ok(())
        }
        McpAction::Remove { name } => {
            let removed = remove_mcp_server(&name)?;
            let credential_removed = bob_core::auth::AuthStore::remove_credential(&format!("mcp:{name}"))
                .await
                .with_context(|| format!("MCP configuration removal completed, but credentials for '{name}' could not be removed; correct the credential-store error and retry bob mcp remove {name}"))?;
            if removed || credential_removed {
                println!("\x1b[32mremoved\x1b[0m MCP server '{}'", name);
            } else {
                println!("no MCP server named '{}'", name);
            }
            Ok(())
        }
    }
}

fn run_lsp(action: LspAction) -> anyhow::Result<()> {
    use bob_core::core::config::{
        add_lsp_server, list_lsp_servers, remove_lsp_server, LspServerConfig,
    };
    let cwd = std::env::current_dir()?;
    match action {
        LspAction::Add {
            name,
            ext,
            root,
            command,
        } => {
            let mut parts = command.into_iter();
            let cmd = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("no command given (put it after `--`)"))?;
            let args: Vec<String> = parts.collect();
            let extensions: Vec<String> = ext
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if extensions.is_empty() {
                anyhow::bail!("--ext must list at least one extension, e.g. --ext rs");
            }
            let replaced = add_lsp_server(
                &cwd,
                LspServerConfig {
                    name: name.clone(),
                    command: cmd,
                    args,
                    extensions,
                    root,
                },
            )?;
            println!(
                "\x1b[32m{}\x1b[0m LSP server '{}' in ./.bob.config.toml",
                if replaced { "updated" } else { "added" },
                name
            );
            Ok(())
        }
        LspAction::List => {
            let servers = list_lsp_servers(&cwd)?;
            if servers.is_empty() {
                println!("no language servers configured for this project. Add one with:");
                println!("  \x1b[36mbob lsp add rust --ext rs -- rust-analyzer\x1b[0m");
                return Ok(());
            }
            println!("language servers (./.bob.config.toml):");
            for s in servers {
                let args = if s.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", s.args.join(" "))
                };
                println!(
                    "  \x1b[1m{}\x1b[0m  \x1b[90m{}{}\x1b[0m  \x1b[36m[{}]\x1b[0m  root={}",
                    s.name,
                    s.command,
                    args,
                    s.extensions.join(","),
                    s.root
                );
            }
            Ok(())
        }
        LspAction::Remove { name } => {
            if remove_lsp_server(&cwd, &name)? {
                println!("\x1b[32mremoved\x1b[0m LSP server '{}'", name);
            } else {
                println!("no LSP server named '{}'", name);
            }
            Ok(())
        }
    }
}

fn wait_dots() {
    print!("waiting for authorization");
    let _ = std::io::stdout().flush();
}
fn dot() {
    print!(".");
    let _ = std::io::stdout().flush();
}
fn done(pretty: &str, id: &str) -> anyhow::Result<()> {
    let selected = bob_core::core::config::set_default_provider_if_unset(id).with_context(|| {
        format!("Logged in to {pretty} and saved credentials, but could not save the default provider. Use bob --provider {id}")
    })?;
    if selected {
        println!(
            "\n\x1b[32m✓ logged in to {}.\x1b[0m Set as your default provider. Start with:  bob",
            pretty
        );
    } else {
        println!(
            "\n\x1b[32m✓ logged in to {}.\x1b[0m Use it with:  bob --provider {}",
            pretty, id
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod login_tests;
