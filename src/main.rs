mod cache;
mod config;
mod gateway;
mod http;
mod interpolate;
mod oauth;
mod platform;
mod search;
mod upstream;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::future::join_all;
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::{
    config::{Config, Mode, Overrides},
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
    /// Connect to all servers, print their tools and exit
    Tools,
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(Config::default_path);
    let mut cfg = Config::load(&path)?;
    let overrides = Overrides {
        mode: cli.mode,
        idle_timeout_secs: cli.idle_timeout,
    };

    match cli.command {
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
        Command::Tools => {
            let hub = Hub::new(cfg, overrides);
            let state = hub.state();
            let listings = join_all(
                state
                    .upstreams()
                    .iter()
                    .map(|u| async move { (u, u.refresh_tools().await) }),
            )
            .await;
            for (upstream, result) in listings {
                let mode = upstream.cfg.mode(state.default_mode);
                println!("[{}] mode={mode:?}", upstream.name);
                match result {
                    Ok(tools) => {
                        for tool in tools.iter() {
                            let desc = tool.description.as_deref().unwrap_or("");
                            let desc = desc.lines().next().unwrap_or("");
                            println!(
                                "  {}  {desc}",
                                state.qualified_name(&upstream.name, &tool.name)
                            );
                        }
                    }
                    Err(e) => println!("  ERROR: {e:#}"),
                }
            }
            hub.shutdown().await;
        }
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
