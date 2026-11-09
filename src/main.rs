mod cache;
mod config;
mod edit;
mod gateway;
mod http;
mod interpolate;
mod list;
mod oauth;
mod platform;
mod search;
mod upstream;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{ArgGroup, Parser, Subcommand};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    cache::ToolStore,
    config::{Config, Mode, Overrides},
    edit::NewServer,
    gateway::{Gateway, Hub, Scope, SearchArgs},
};

/// Minimal MCP gateway: aggregates stdio and HTTP MCP servers behind one endpoint.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/emcipi/config.toml]
    #[arg(short, long, env = "EMCIPI_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Default mode for servers without a pinned `mode` (overrides the config)
    #[arg(short, long, env = "EMCIPI_MODE", global = true, value_enum)]
    mode: Option<Mode>,

    /// Stop idle stdio servers after this many seconds, 0 = never (overrides the config)
    #[arg(long, env = "EMCIPI_IDLE_TIMEOUT", global = true, value_name = "SECS")]
    idle_timeout: Option<u64>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP over stdin/stdout (one gateway per client)
    Stdio,
    /// Serve MCP over Streamable HTTP (one gateway shared by all clients)
    Http {
        /// Overrides http.listen
        #[arg(short, long, env = "EMCIPI_LISTEN")]
        listen: Option<String>,
        /// Bearer token clients must send; overrides http.auth_token. Prefer the env var.
        #[arg(long, env = "EMCIPI_AUTH_TOKEN", hide_env_values = true)]
        auth_token: Option<String>,
    },
    /// Show the configured servers, or the tool names of one server
    List {
        /// Only list the tool names of this server
        server: Option<String>,
        /// Connect to the servers and update the cached tool lists
        #[arg(short, long)]
        refresh: bool,
    },
    /// Add a server to the config: `add NAME --url URL` or `add NAME -- COMMAND ARGS...`
    #[command(group(ArgGroup::new("transport").required(true).args(["url", "command"])))]
    Add {
        /// Server name; also the prefix of its tools
        name: String,
        /// URL of a Streamable HTTP server
        #[arg(long)]
        url: Option<String>,
        /// Program and arguments of a stdio server, after `--`
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<String>,
        /// Environment variable of a stdio server; use placeholders for secrets
        #[arg(short, long = "env", value_name = "KEY=VALUE", value_parser = edit::parse_env)]
        env: Vec<(String, String)>,
        /// HTTP header; use placeholders for secrets
        #[arg(short = 'H', long = "header", value_name = "NAME: VALUE", value_parser = edit::parse_header)]
        headers: Vec<(String, String)>,
        /// Always list this server's tools directly
        #[arg(long, conflicts_with = "dynamic")]
        r#static: bool,
        /// Always hide this server's tools behind the tool search
        #[arg(long)]
        dynamic: bool,
        /// What the server is good for, shown to the model in dynamic mode
        #[arg(short, long)]
        description: Option<String>,
        /// Add the server disabled
        #[arg(long)]
        disabled: bool,
        /// Replace a server with the same name
        #[arg(short, long)]
        force: bool,
    },
    /// Remove a server from the config, with its cached tools and stored login
    Remove { server: String },
    /// Sign in to an OAuth-protected HTTP server and store its tokens
    Login {
        server: String,
        /// Only print the URL instead of opening the browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Delete the stored tokens of a server
    Logout { server: String },
    /// Run a tool search across all servers, as the model would see it
    Search {
        /// Keywords; empty lists the servers
        #[arg(default_value = "")]
        query: String,
        /// Only search this server
        #[arg(short, long)]
        server: Option<String>,
        #[arg(short, long)]
        limit: Option<usize>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // `list` reports connection errors in its own output.
    let default_filter = match cli.command {
        Command::List { .. } => "off",
        _ => "info,rmcp=warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let path = cli.config.unwrap_or_else(Config::default_path);
    let overrides = Overrides {
        mode: cli.mode,
        idle_timeout_secs: cli.idle_timeout,
    };

    // These edit the file itself, so they also work when it is missing or invalid.
    match cli.command {
        Command::Add {
            name,
            url,
            command,
            env,
            headers,
            r#static,
            dynamic,
            description,
            disabled,
            force,
        } => {
            let server = NewServer {
                url,
                command,
                env,
                headers,
                mode: (r#static || dynamic).then_some(if dynamic {
                    Mode::Dynamic
                } else {
                    Mode::Static
                }),
                description,
                disabled,
            };
            for secret in edit::literal_secrets(&server) {
                eprintln!(
                    "warning: {secret} is stored in plain text; consider a placeholder such as '${{cmd:...}}'"
                );
            }
            let replaced = edit::add(&path, &name, &server, force)?;
            let verb = if replaced { "Replaced" } else { "Added" };
            let prep = if replaced { "in" } else { "to" };
            eprintln!("{verb} '{name}' {prep} {}.", path.display());
            if !disabled {
                eprintln!("Test it with `emcipi list {name}`.");
            }
            return Ok(());
        }
        Command::Remove { server } => {
            edit::remove(&path, &server)?;
            let cache = ToolStore::new(platform::default_cache_dir()).remove(&server)?;
            let login = oauth::logout(&server)?;
            eprintln!("Removed '{server}' from {}.", path.display());
            let deleted = match (cache, login) {
                (true, true) => "Deleted its cached tools and stored login.",
                (true, false) => "Deleted its cached tools.",
                (false, true) => "Deleted its stored login.",
                (false, false) => "",
            };
            if !deleted.is_empty() {
                eprintln!("{deleted}");
            }
            return Ok(());
        }
        _ => {}
    }

    let mut cfg = Config::load(&path)?;
    match cli.command {
        Command::Add { .. } | Command::Remove { .. } => unreachable!("handled above"),
        Command::Stdio => {
            let hub = Hub::new(cfg, overrides);
            hub.warm_up();
            hub.watch_config(path, None);
            let gateway = Gateway {
                hub: hub.clone(),
                mode: None,
            };
            let service = gateway.serve(rmcp::transport::stdio()).await?;
            tokio::select! {
                _ = service.waiting() => {}
                _ = shutdown_signal() => {}
            }
            hub.shutdown().await;
        }
        Command::Http { listen, auth_token } => {
            let http_from_file = cfg.http.clone();
            if let Some(listen) = listen {
                cfg.http.listen = listen;
            }
            let configured = auth_token.filter(|t| !t.is_empty());
            let auth_token = match configured.or(cfg.http.auth_token.take()) {
                Some(raw) => {
                    let token = interpolate::resolve(&raw, &cfg.base_dir)
                        .await
                        .context("http.auth_token")?;
                    anyhow::ensure!(
                        !token.is_empty(),
                        "http.auth_token resolved to an empty string"
                    );
                    Some(token)
                }
                None => None,
            };
            let http_cfg = std::mem::take(&mut cfg.http);
            let hub = Hub::new(cfg, overrides);
            hub.warm_up();
            hub.watch_config(path, Some(http_from_file));
            let shutdown = CancellationToken::new();
            let on_signal = shutdown.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                on_signal.cancel();
            });
            let opts = http::HttpOptions {
                cfg: &http_cfg,
                auth_token,
            };
            let result = http::serve(hub.clone(), opts, shutdown).await;
            hub.shutdown().await;
            result?;
        }
        Command::Login { server, no_browser } => {
            let upstream = cfg
                .servers
                .get(&server)
                .with_context(|| format!("no server '{server}' in {}", path.display()))?;
            oauth::login(&server, upstream, &cfg.base_dir, !no_browser).await?;
        }
        Command::Logout { server } => {
            if oauth::logout(&server)? {
                eprintln!("Signed out of '{server}'.");
            } else {
                eprintln!("No stored login for '{server}'.");
            }
        }
        Command::Search {
            query,
            server,
            limit,
        } => {
            let hub = Hub::new(cfg, overrides);
            let args = SearchArgs {
                query,
                server,
                limit,
                include_schema: false,
            };
            println!("{}", hub.state().search(args, Scope::AllServers).await);
            hub.shutdown().await;
        }
        Command::List { server, refresh } => match server {
            Some(server) => list::tools(cfg, overrides, &server, refresh).await?,
            None => list::servers(cfg, overrides, refresh).await?,
        },
    }
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(windows)]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
