mod tui;

use bob_core::core::config::load_config;
use bob_core::core::session::{list_sessions, load_session, new_session, Session};
use bob_core::providers::create_provider;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "bob",
    about = "A provider-agnostic, multi-agent coding assistant.",
    version,
    // No subcommand → start a chat with the top-level options below.
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Provider id: copilot | anthropic | openai.
    #[arg(short = 'p', long, global = true)]
    provider: Option<String>,

    /// Model id within the provider (e.g. gpt-5, claude-sonnet-4-5).
    #[arg(short = 'm', long, global = true)]
    model: Option<String>,

    /// Resume a session by id; omit the id to resume the most recent.
    #[arg(long, visible_alias = "restore", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
}

#[derive(Subcommand)]
enum Command {
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
    /// Show the effective configuration and where it comes from.
    Config,
    /// Manage MCP servers (stored in ~/.bob/settings.json).
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
}

#[derive(Subcommand)]
enum McpAction {
    /// Add a stdio MCP server. Put the command after `--`.
    ///
    /// e.g.  bob mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem /path
    Add {
        /// A name for the server (namespaces its tools as <name>.<tool>).
        name: String,
        /// Repeatable env var, KEY=VALUE.
        #[arg(short, long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// The command and its args (everything after `--`).
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List configured MCP servers.
    List,
    /// Remove an MCP server by name.
    Remove {
        name: String,
    },
}

fn now_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn make_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Interactive session picker shown for a bare `--resume`. Prints the stored
/// sessions (same list bob-remote's drawer shows, via `list_sessions`) and lets
/// the user choose one by number. Enter/empty or `n` starts a new session.
fn pick_session() -> anyhow::Result<Option<Session>> {
    let summaries = list_sessions();
    if summaries.is_empty() {
        return Ok(None); // nothing to resume → caller creates a fresh one
    }
    println!("\x1b[1mResume a session:\x1b[0m");
    for (i, s) in summaries.iter().enumerate() {
        println!(
            "  \x1b[36m{:>2}\x1b[0m  {}  \x1b[90m({} msgs · {})\x1b[0m",
            i + 1,
            s.title,
            s.message_count,
            s.provider,
        );
    }
    print!("\nnumber to resume, or Enter for a new session: ");
    std::io::stdout().flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let choice = line.trim();
    if choice.is_empty() || choice.eq_ignore_ascii_case("n") {
        return Ok(None);
    }
    match choice.parse::<usize>() {
        Ok(n) if n >= 1 && n <= summaries.len() => {
            let id = &summaries[n - 1].id;
            Ok(load_session(id)?)
        }
        _ => {
            println!("no such session; starting a new one.");
            Ok(None)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Login { provider }) => return run_login(&provider).await,
        Some(Command::Logout { provider }) => return run_logout(&provider),
        Some(Command::Auth) => return show_auth(),
        Some(Command::Config) => return show_config(),
        Some(Command::Mcp { action }) => return run_mcp(action),
        None => {} // fall through to a chat
    }

    let cwd: PathBuf = std::env::current_dir()?;
    let config = load_config(&cwd)?;

    // Resolve the provider spec: --provider/--model override config; a bare
    // colon-form (--provider openai:gpt-5) is also accepted for convenience.
    let provider_spec = resolve_provider_spec(&cli, &config.provider);

    let provider = match create_provider(&provider_spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("\x1b[31m{}\x1b[0m\n", e);
            print_onboarding();
            std::process::exit(1);
        }
    };

    // Resolve or create the session.
    //   --resume <id>  → load that session
    //   --resume       → interactive picker over all stored sessions
    //   (no flag)      → fresh session
    let session = match cli.resume {
        Some(ref id) if !id.is_empty() => load_session(id)?,
        Some(_) => pick_session()?,
        None => None,
    }
    .unwrap_or_else(|| new_session(provider.name(), make_id(), now_stamp()));
    let session_id = session.id.clone();

    tui::run(config, provider, provider_spec, cwd, session).await?;
    println!("resume this session with:  bob --resume {}", session_id);
    Ok(())
}

/// Combine --provider/--model (and the colon shorthand / config default) into a
/// single "provider:model" spec.
fn resolve_provider_spec(cli: &Cli, config_default: &str) -> String {
    // Base provider: explicit flag, else the config default's provider part.
    let base = cli
        .provider
        .clone()
        .unwrap_or_else(|| config_default.to_string());
    let (prov, colon_model) = match base.split_once(':') {
        Some((p, m)) => (p.to_string(), Some(m.to_string())),
        None => (base, None),
    };
    // Model precedence: --model flag > colon form > config default's model.
    let model = cli.model.clone().or(colon_model).or_else(|| {
        config_default
            .split_once(':')
            .map(|(_, m)| m.to_string())
    });
    match model {
        Some(m) if !m.is_empty() => format!("{}:{}", prov, m),
        _ => prov,
    }
}

/// Interactive login flow, dispatched per provider.
async fn run_login(which: &str) -> anyhow::Result<()> {
    match which {
        "copilot" | "github" => {
            let device = bob_core::auth::copilot::begin_login().await?;
            println!("\nTo authorize bob with GitHub Copilot:");
            println!("  1. open  \x1b[36m{}\x1b[0m", device.verification_uri);
            println!("  2. enter code  \x1b[1m{}\x1b[0m\n", device.user_code);
            wait_dots();
            bob_core::auth::copilot::finish_login(&device, dot).await?;
            done("Copilot", "copilot");
            Ok(())
        }
        "anthropic" | "claude" => {
            let handle = bob_core::auth::anthropic::begin_login();
            println!("\nTo authorize bob with your Claude (Pro/Max) subscription:");
            println!("  open this URL in your browser:\n  \x1b[36m{}\x1b[0m\n", handle.url);
            println!("waiting for you to approve in the browser…");
            bob_core::auth::anthropic::finish_login(handle).await?;
            done("Anthropic", "anthropic");
            Ok(())
        }
        "openai" | "chatgpt" => {
            let device = bob_core::auth::openai::begin_login().await?;
            println!("\nTo authorize bob with your ChatGPT (Plus/Pro) subscription:");
            println!("  1. open  \x1b[36m{}\x1b[0m", device.verification_uri);
            println!("  2. enter code  \x1b[1m{}\x1b[0m\n", device.user_code);
            wait_dots();
            bob_core::auth::openai::finish_login(&device, dot).await?;
            done("OpenAI", "openai");
            Ok(())
        }
        other => anyhow::bail!("unknown provider '{}'. known: copilot, anthropic, openai", other),
    }
}

fn run_logout(which: &str) -> anyhow::Result<()> {
    let id = match which {
        "github" => "copilot",
        "claude" => "anthropic",
        "chatgpt" => "openai",
        other => other,
    };
    let mut store = bob_core::auth::AuthStore::load();
    if store.remove(id) {
        store.save()?;
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
    // Note any API keys present in the environment.
    for (env, prov) in [("ANTHROPIC_API_KEY", "anthropic"), ("OPENAI_API_KEY", "openai")] {
        if std::env::var(env).is_ok() {
            println!("  \x1b[90m{} is set (api key for {})\x1b[0m", env, prov);
        }
    }
    Ok(())
}

fn show_config() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = load_config(&cwd)?;
    println!("effective config:");
    println!("  provider:    {}", config.provider);
    println!(
        "  system:      {}",
        if config.system.is_some() { "(custom)" } else { "(default bob prompt)" }
    );
    println!("  max_turns:   {}", config.max_turns.unwrap_or(20));
    println!("  permissions: default={}", config.permissions.default);
    println!("  mcp_servers: {}", config.mcp_servers.len());
    println!("\nsources (later overrides earlier):");
    println!("  1. built-in defaults");
    println!("  2. ~/.bob/settings.json");
    println!("  3. ./.bob/config.json  (or ./bob.config.json)");
    Ok(())
}

fn run_mcp(action: McpAction) -> anyhow::Result<()> {
    use bob_core::core::config::{
        add_mcp_server, list_mcp_servers, remove_mcp_server, McpServerConfig,
    };
    match action {
        McpAction::Add { name, env, command } => {
            let mut parts = command.into_iter();
            let cmd = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("no command given (put it after `--`)"))?;
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
            })?;
            println!(
                "\x1b[32m{}\x1b[0m MCP server '{}' in ~/.bob/settings.json",
                if replaced { "updated" } else { "added" },
                name
            );
            Ok(())
        }
        McpAction::List => {
            let servers = list_mcp_servers()?;
            if servers.is_empty() {
                println!("no MCP servers configured. Add one with:");
                println!("  \x1b[36mbob mcp add <name> -- <command> [args...]\x1b[0m");
                return Ok(());
            }
            println!("MCP servers (~/.bob/settings.json):");
            for s in servers {
                let args = if s.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", s.args.join(" "))
                };
                println!("  \x1b[1m{}\x1b[0m  \x1b[90m{}{}\x1b[0m", s.name, s.command, args);
            }
            Ok(())
        }
        McpAction::Remove { name } => {
            if remove_mcp_server(&name)? {
                println!("\x1b[32mremoved\x1b[0m MCP server '{}'", name);
            } else {
                println!("no MCP server named '{}'", name);
            }
            Ok(())
        }
    }
}

fn print_onboarding() {
    println!("No usable provider. Get started with one of:");
    println!("  \x1b[36mbob login copilot\x1b[0m     use GitHub Copilot");
    println!("  \x1b[36mbob login openai\x1b[0m      use your ChatGPT subscription");
    println!("  \x1b[36mbob login anthropic\x1b[0m   use your Claude subscription");
    println!("or set an API key: ANTHROPIC_API_KEY / OPENAI_API_KEY");
    println!("then pick one with:  \x1b[36mbob --provider <name>\x1b[0m");
}

fn wait_dots() {
    print!("waiting for authorization");
    let _ = std::io::stdout().flush();
}
fn dot() {
    print!(".");
    let _ = std::io::stdout().flush();
}
fn done(pretty: &str, id: &str) {
    println!("\n\x1b[32m✓ logged in to {}.\x1b[0m Use it with:  bob --provider {}", pretty, id);
}
