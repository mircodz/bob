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
    /// Host this session for phone control: dial the relay and wait for your
    /// phone to pair. Prints a pairing QR the phone scans (once per device).
    Remote {
        /// Relay WebSocket URL (defaults to the configured/public relay).
        #[arg(long)]
        relay: Option<String>,
        /// Pairing session id (default: auto-generated).
        #[arg(long)]
        session: Option<String>,
        /// Provider spec, e.g. anthropic:claude-sonnet-4-5 (defaults to config).
        #[arg(short = 'p', long)]
        provider: Option<String>,
        /// Debug: run a terminal controller instead of the agent host. Requires
        /// --pair-url (the bobpair:// URL printed by a running host).
        #[arg(long, hide = true)]
        test_client: bool,
        /// Debug: the bobpair:// URL for the test-client to pair with.
        #[arg(long, hide = true)]
        pair_url: Option<String>,
    },
    /// Run the public relay that pairs a `bob remote` host with your phone.
    Relay {
        /// Address to bind, e.g. 0.0.0.0:8787.
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Write a commented default config to ~/.bob/config.toml.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
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
    Remove { name: String },
}

#[derive(Subcommand)]
enum LspAction {
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
        Some(Command::Config { action }) => return run_config(action),
        Some(Command::Mcp { action }) => return run_mcp(action),
        Some(Command::Lsp { action }) => return run_lsp(action),
        Some(Command::Remote {
            relay,
            session,
            provider,
            test_client,
            pair_url,
        }) => return run_remote(relay, session, provider, test_client, pair_url).await,
        Some(Command::Relay { addr }) => return run_relay(addr).await,
        None => {} // fall through to a chat
    }

    let cwd: PathBuf = std::env::current_dir()?;
    let config = load_config(&cwd)?;

    // Resolve the provider spec: --provider/--model override config; a bare
    // colon-form (--provider openai:gpt-5) is also accepted for convenience.
    let provider_spec = resolve_provider_spec(&cli, &config);

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
fn resolve_provider_spec(cli: &Cli, config: &bob_core::core::config::BobConfig) -> String {
    // Base provider: explicit flag, else the config `provider` (which may carry a
    // colon model form).
    let base = cli
        .provider
        .clone()
        .unwrap_or_else(|| config.provider.clone());
    let (prov, colon_model) = match base.split_once(':') {
        Some((p, m)) => (p.to_string(), Some(m.to_string())),
        None => (base, None),
    };
    // Model precedence: --model flag > config `model` field > colon form.
    let cfg_model = if config.model.is_empty() {
        None
    } else {
        Some(config.model.clone())
    };
    let model = cli.model.clone().or(cfg_model).or(colon_model);
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
            println!(
                "  open this URL in your browser:\n  \x1b[36m{}\x1b[0m\n",
                handle.url
            );
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
        other => anyhow::bail!(
            "unknown provider '{}'. known: copilot, anthropic, openai",
            other
        ),
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
    println!("\nsources (later overrides earlier):");
    println!("  1. built-in defaults");
    println!("  2. ~/.bob/config.toml");
    println!("  3. ./.bob.config.toml");
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
                "\x1b[32m{}\x1b[0m MCP server '{}' in ~/.bob/config.toml",
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
            println!("MCP servers (~/.bob/config.toml):");
            for s in servers {
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

/// Resolve remote settings and run the phone-control host (or the debug
/// Host this session for phone control (or, with --test-client, run the debug
/// controller). The host loads its long-term identity, generates a fresh pairing
/// secret, and prints a `bobpair://` URL (as a scannable block) the phone uses
/// once. After the Noise handshake the safety number is shown and the phone's
/// key is remembered in ~/.bob/devices.toml, so future connects need no pairing.
async fn run_remote(
    relay: Option<String>,
    session: Option<String>,
    provider: Option<String>,
    test_client: bool,
    pair_url: Option<String>,
) -> anyhow::Result<()> {
    use bob_secure::{admission, Device, DeviceBook, Identity, Pairing};

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let bob_dir = home.join(".bob");

    // --- Debug test-client: pair from a bobpair:// URL and drive the host. ---
    if test_client {
        let url = pair_url
            .ok_or_else(|| anyhow::anyhow!("--test-client requires --pair-url <bobpair://…>"))?;
        let pairing = Pairing::from_url(&url)?;
        let identity = Identity::generate(); // ephemeral identity for the debug client
        let params = bob_remote::SecureParams {
            identity: identity.secret(),
            ephemeral: Identity::generate().secret(),
            pairing_secret: pairing.secret.clone(),
            peer_static: Some(pairing.static_key),
            on_established: Box::new(|_| {}),
        };
        return bob_remote::client::run(pairing.relay, pairing.session, params).await;
    }

    // --- Host: identity + pairing secret + QR, then run the agent host. ---
    let relay = relay.unwrap_or_else(|| "ws://127.0.0.1:8787/ws".to_string());
    let session = session.unwrap_or_else(make_id);
    let identity = Identity::load_or_create(&bob_dir.join("identity.key"))?;

    // A fresh pairing secret per run; the phone captures it from the QR.
    let pairing_secret = make_id().replace('-', "").into_bytes();
    let pairing = Pairing {
        relay: relay.clone(),
        session: session.clone(),
        static_key: identity.public(),
        secret: pairing_secret.clone(),
    };
    print_pairing(&pairing);
    let _ = admission::prove(&pairing_secret, &session); // (proof computed in host::run)

    let devices_path = bob_dir.join("devices.toml");
    let session_for_cb = session.clone();
    let params = bob_remote::SecureParams {
        identity: identity.secret(),
        ephemeral: Identity::generate().secret(),
        pairing_secret,
        peer_static: None, // host learns the phone's key during the handshake
        on_established: Box::new(move |est| {
            println!(
                "\n\x1b[32m✓ phone connected.\x1b[0m Safety number: \x1b[1m{:04}\x1b[0m",
                est.safety_number
            );
            println!("  confirm it matches the number shown on your phone.");
            // Trust-on-first-use: remember this phone so future connects skip pairing.
            if let Ok(mut book) = DeviceBook::load(&devices_path) {
                if !book.is_trusted(&est.peer_static) {
                    let name = format!("phone-{}", &session_for_cb[..8.min(session_for_cb.len())]);
                    let _ = book.add(Device::new(name, &est.peer_static, now_stamp()));
                }
            }
        }),
    };
    bob_remote::host::run(relay, session, params, provider).await
}

/// Print the pairing bundle the phone scans — a `bobpair://` URL plus the
/// human-readable parts, so it works whether the app scans a QR or you type it.
fn print_pairing(pairing: &bob_secure::Pairing) {
    println!("\n\x1b[1mPair your phone\x1b[0m — scan this in the Bob Remote app (once):");
    println!("  \x1b[36m{}\x1b[0m", pairing.to_url());
    println!("\nwaiting for your phone to connect…\n");
}

/// Run the public relay that pairs a `bob remote` host with a phone. The relay
/// holds no secret: it pairs two peers that present matching admission proofs and
/// forwards their end-to-end-encrypted frames without being able to read them.
async fn run_relay(addr: String) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("\x1b[32mrelay listening on\x1b[0m {}", addr);
    axum::serve(listener, bob_relay::router()).await?;
    Ok(())
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
    println!(
        "\n\x1b[32m✓ logged in to {}.\x1b[0m Use it with:  bob --provider {}",
        pretty, id
    );
}
