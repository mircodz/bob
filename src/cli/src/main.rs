mod tui;

use bob_core::core::config::load_config;
use bob_core::core::session::{
    latest_session_in, list_sessions, list_sessions_in, load_session, new_session, session_preview,
    Session, SessionSummary,
};
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

    /// Resume a session by id; omit the id to pick from sessions in THIS directory.
    #[arg(long, visible_alias = "restore", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    /// Continue the most recent session in the current directory (no picker).
    #[arg(short = 'c', long)]
    r#continue: bool,
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

/// A short human "time ago" for a unix-seconds timestamp string, so same-titled
/// sessions in the picker are distinguishable (e.g. "3m ago", "2h ago").
fn time_ago(updated_at: &str) -> String {
    let then: u64 = updated_at.parse().unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(then);
    if then == 0 {
        "?".to_string()
    } else if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Case-insensitive subsequence match: every char of `query` appears in `hay` in
/// order (not necessarily adjacent). Empty query matches everything. Powers the
/// picker's type-to-filter.
fn fuzzy_match(query: &str, hay: &str) -> bool {
    let mut q = query.chars().flat_map(char::to_lowercase).peekable();
    if q.peek().is_none() {
        return true;
    }
    for h in hay.chars().flat_map(char::to_lowercase) {
        if q.peek() == Some(&h) {
            q.next();
        }
    }
    q.peek().is_none()
}

/// A session plus its cached preview lines, for the picker.
struct PickerItem {
    summary: SessionSummary,
    preview: Vec<String>,
    /// Lowercased title + preview, precomputed for fuzzy filtering.
    haystack: String,
}

/// Interactive session picker shown for a bare `--resume`. Renders an arrow-key
/// selectable list (↑/↓ or j/k) with a preview of each conversation's tail; typing
/// fuzzy-filters by title + preview. Enter opens the highlighted session; Esc (or an
/// empty list) starts a new one. Sessions are scoped to the current directory, with
/// a fallback to all sessions for legacy entries saved without a cwd.
fn pick_session(cwd: &str) -> anyhow::Result<Option<Session>> {
    let mut summaries = list_sessions_in(cwd);
    if summaries.is_empty() {
        summaries = list_sessions();
    }
    if summaries.is_empty() {
        return Ok(None); // nothing to resume → caller creates a fresh one
    }

    // Cache each session's preview (last 3 messages) up front.
    let items: Vec<PickerItem> = summaries
        .into_iter()
        .map(|summary| {
            let preview = session_preview(&summary.id, 3);
            let haystack = format!("{} {}", summary.title, preview.join(" ")).to_lowercase();
            PickerItem {
                summary,
                preview,
                haystack,
            }
        })
        .collect();

    match run_picker(&items)? {
        Some(idx) => Ok(load_session(&items[idx].summary.id)?),
        None => Ok(None),
    }
}

/// Drive the full-screen ratatui picker, returning the chosen index into `items`
/// (or None for "start a new session"). Runs on the alternate screen and restores
/// the terminal before returning, so it never scrolls the user's scrollback — the
/// list is windowed to the viewport, so it handles hundreds of sessions cleanly.
fn run_picker(items: &[PickerItem]) -> anyhow::Result<Option<usize>> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Restore the terminal from one place, even on an early `?` return.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        }
    }
    let _restore = Restore;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut query = String::new();
    let mut selected = 0usize;
    let mut scroll = 0usize;

    let result = loop {
        // Filter to the indices matching the current query (fuzzy over title+preview).
        let filtered: Vec<usize> = (0..items.len())
            .filter(|&i| fuzzy_match(&query, &items[i].haystack))
            .collect();
        if selected >= filtered.len() {
            selected = filtered.len().saturating_sub(1);
        }

        terminal.draw(|f| draw_picker(f, items, &filtered, selected, &mut scroll, &query))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break None,
            (KeyCode::Enter, _) => break filtered.get(selected).copied(),
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) if !filtered.is_empty() => {
                selected = selected.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE)
                if !filtered.is_empty() =>
            {
                selected = (selected + 1).min(filtered.len() - 1);
            }
            (KeyCode::Backspace, _) => {
                query.pop();
                selected = 0;
            }
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                query.push(c);
                selected = 0;
            }
            _ => {}
        }
    };
    Ok(result)
}

/// Render one frame of the picker: a title, the windowed session list (each row =
/// title + metadata; the selected row highlighted), a preview pane for the selected
/// session, and a filter/hints footer. `scroll` is updated to keep `selected` visible.
fn draw_picker(
    f: &mut ratatui::Frame,
    items: &[PickerItem],
    filtered: &[usize],
    selected: usize,
    scroll: &mut usize,
    query: &str,
) {
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph};

    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Min(3),    // list
            Constraint::Length(6), // preview pane
            Constraint::Length(1), // filter/footer
        ])
        .split(area);

    // Title.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Resume a session",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {} sessions", filtered.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        chunks[0],
    );

    // Windowed list: keep the selection visible within the list area's height.
    let list_h = chunks[1].height as usize;
    if selected < *scroll {
        *scroll = selected;
    } else if list_h > 0 && selected >= *scroll + list_h {
        *scroll = selected + 1 - list_h;
    }
    let mut rows: Vec<Line> = Vec::new();
    if filtered.is_empty() {
        rows.push(Line::from(Span::styled(
            "  no sessions match",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (row, &i) in filtered.iter().enumerate().skip(*scroll).take(list_h) {
        let it = &items[i];
        let s = &it.summary;
        let short_id = s.id.get(..8).unwrap_or(&s.id);
        let sel = row == selected;
        let marker = if sel { "❯ " } else { "  " };
        let title_style = if sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        rows.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(s.title.clone(), title_style),
            Span::styled(
                format!(
                    "  ({} msgs · {} · {} · {})",
                    s.message_count,
                    time_ago(&s.updated_at),
                    short_id,
                    s.provider
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(rows), chunks[1]);

    // Preview pane for the selected session.
    let preview_lines: Vec<Line> = filtered
        .get(selected)
        .map(|&i| &items[i].preview)
        .map(|p| {
            if p.is_empty() {
                vec![Line::from(Span::styled(
                    "(no messages yet)",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                p.iter()
                    .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(Color::Gray))))
                    .collect()
            }
        })
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(preview_lines).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " preview ",
                    Style::default().fg(Color::DarkGray),
                )),
        ),
        chunks[2],
    );

    // Filter/footer.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::DarkGray)),
            Span::styled(query.to_string(), Style::default().fg(Color::White)),
            Span::styled(
                "    ↑/↓ move · type to filter · enter open · esc new",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        chunks[3],
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Login { provider }) => return run_login(&provider).await,
        Some(Command::Logout { provider }) => return run_logout(&provider),
        Some(Command::Auth) => return show_auth(),
        Some(Command::Config { action }) => return run_config(action),
        Some(Command::Mcp { action }) => return run_mcp(action).await,
        Some(Command::Lsp { action }) => return run_lsp(action),
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

    // Resolve or create the session, scoped to the current directory.
    //   --continue      → resume the most recent session in THIS dir (no picker)
    //   --resume <id>   → load that session by id
    //   --resume        → interactive picker over sessions in THIS dir
    //   (no flag)       → fresh session
    let cwd_str = cwd.to_string_lossy().to_string();
    let session = if cli.r#continue {
        latest_session_in(&cwd_str)?
    } else {
        match cli.resume {
            Some(ref id) if !id.is_empty() => load_session(id)?,
            Some(_) => pick_session(&cwd_str)?,
            None => None,
        }
    }
    .unwrap_or_else(|| new_session(provider.name(), make_id(), now_stamp(), cwd_str.clone()));
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

            // Static-token path (e.g. a GitHub Personal Access Token).
            if let Some(token) = token {
                bob_core::auth::mcp::store_static_token(&name, &token)?;
                println!("\x1b[32mstored token\x1b[0m for MCP server '{}'", name);
                return Ok(());
            }

            let url = server.url.clone().unwrap();
            // Discover OAuth config if we don't already have it, and persist it.
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
            if remove_mcp_server(&name)? {
                // Also clear any stored OAuth credentials for this server.
                let mut store = bob_core::auth::AuthStore::load();
                if store.remove(&format!("mcp:{}", name)) {
                    let _ = store.save();
                }
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

#[cfg(test)]
mod tests {
    use super::{fuzzy_match, Cli, Command};
    use clap::Parser;

    #[test]
    fn cli_rejects_removed_subcommands() {
        for command in ["remote", "relay"] {
            let error = Cli::try_parse_from(["bob", command]).err().unwrap();
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
        }
    }

    #[test]
    fn cli_accepts_core_subcommands() {
        for args in [
            vec!["bob", "login", "copilot"],
            vec!["bob", "logout", "copilot"],
            vec!["bob", "auth"],
            vec!["bob", "config"],
            vec!["bob", "config", "init"],
            vec!["bob", "mcp", "list"],
            vec!["bob", "lsp", "list"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert!(matches!(
                cli.command,
                Some(
                    Command::Login { .. }
                        | Command::Logout { .. }
                        | Command::Auth
                        | Command::Config { .. }
                        | Command::Mcp { .. }
                        | Command::Lsp { .. }
                )
            ));
        }
        assert!(Cli::try_parse_from(["bob"]).unwrap().command.is_none());
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("fb", "foo bar")); // subsequence, not adjacent
        assert!(fuzzy_match("BAR", "foo bar")); // case-insensitive
        assert!(fuzzy_match("foobar", "foo bar")); // spaces skipped as gaps
        assert!(!fuzzy_match("baz", "foo bar")); // 'z' absent
        assert!(!fuzzy_match("rab", "foo bar")); // wrong order
    }
}
