use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use axum::http::{HeaderName, HeaderValue};
use process_wrap::tokio::CommandWrap;
use rmcp::{
    ClientHandler, RoleClient, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, Implementation,
        ProtocolVersion, ServerNotification, SubscriptionFilter, Tool,
    },
    service::{NotificationContext, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::{
    config::{ServerConfig, Transport},
    platform::{self, ProcessTree},
};

/// After a failed connect, further attempts are suppressed for this long so that
/// a broken server does not stall every `tools/list`.
const RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// How long a stopping server gets after stdin is closed, and again after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(3);

type Client = RunningService<RoleClient, UpstreamHandler>;
type ToolCache = Arc<RwLock<Option<Arc<Vec<Tool>>>>>;

pub struct Upstream {
    pub name: String,
    pub cfg: ServerConfig,
    base_dir: PathBuf,
    startup_timeout: Duration,
    idle_timeout: Option<Duration>,
    conn: Mutex<ConnState>,
    /// Survives idle stops, so listing and searching tools does not respawn the server.
    tools: ToolCache,
    tools_changed: broadcast::Sender<()>,
    in_flight: AtomicUsize,
    last_used: std::sync::Mutex<Instant>,
    /// `instructions` from the server's initialize result; kept across idle stops.
    instructions: std::sync::Mutex<Option<String>>,
}

#[derive(Default)]
struct ConnState {
    conn: Option<Connection>,
    last_failure: Option<(Instant, String)>,
}

struct Connection {
    client: Arc<Client>,
    /// Process tree of a stdio server.
    process_tree: Option<ProcessTree>,
}

pub struct UpstreamOptions {
    pub base_dir: PathBuf,
    pub startup_timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub tools_changed: broadcast::Sender<()>,
}

impl Upstream {
    pub fn new(name: String, cfg: ServerConfig, opts: UpstreamOptions) -> Self {
        Self {
            name,
            cfg,
            base_dir: opts.base_dir,
            startup_timeout: opts.startup_timeout,
            idle_timeout: opts.idle_timeout,
            conn: Mutex::default(),
            tools: ToolCache::default(),
            tools_changed: opts.tools_changed,
            in_flight: AtomicUsize::new(0),
            last_used: std::sync::Mutex::new(Instant::now()),
            instructions: std::sync::Mutex::new(None),
        }
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.idle_timeout
    }

    pub fn instructions(&self) -> Option<String> {
        self.instructions.lock().expect("instructions lock").clone()
    }

    /// Tools after include/exclude filtering, with their original (unprefixed) names.
    pub async fn tools(&self) -> Result<Arc<Vec<Tool>>> {
        if let Some(tools) = self.tools.read().await.clone() {
            return Ok(tools);
        }
        let _busy = self.busy();
        let client = self.client().await?;
        let tools: Vec<Tool> = client
            .list_all_tools()
            .await
            .with_context(|| format!("[{}] tools/list failed", self.name))?
            .into_iter()
            .filter(|t| self.cfg.tool_allowed(&t.name))
            .collect();
        let tools = Arc::new(tools);
        *self.tools.write().await = Some(tools.clone());
        Ok(tools)
    }

    pub async fn call(&self, params: CallToolRequestParams) -> Result<CallToolResponse> {
        if !self.cfg.tool_allowed(&params.name) {
            return Err(anyhow!(
                "tool '{}' is disabled by the gateway config",
                params.name
            ));
        }
        let _busy = self.busy();
        let client = self.client().await?;
        client.call_tool_once(params).await.map_err(|e| match e {
            ServiceError::McpError(e) => anyhow!("{}", e.message),
            other => anyhow!(other),
        })
    }

    /// Stops the server if it has been unused for `idle_timeout` and nothing is running.
    pub async fn stop_if_idle(&self) {
        let Some(idle) = self.idle_timeout else {
            return;
        };
        let mut state = self.conn.lock().await;
        if state.conn.is_none()
            || self.in_flight.load(Ordering::SeqCst) > 0
            || self.last_used().elapsed() < idle
        {
            return;
        }
        if let Some(conn) = state.conn.take() {
            tracing::info!(server = %self.name, "idle for {idle:?}, stopping");
            tokio::spawn(stop(conn));
        }
    }

    pub async fn shutdown(&self) {
        let conn = self.conn.lock().await.conn.take();
        if let Some(conn) = conn {
            stop(conn).await;
        }
    }

    fn last_used(&self) -> Instant {
        *self.last_used.lock().expect("last_used lock")
    }

    /// Counted before connecting, so an idle stop cannot race with a starting call.
    fn busy(&self) -> BusyGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        BusyGuard(self)
    }

    async fn client(&self) -> Result<Arc<Client>> {
        let mut state = self.conn.lock().await;
        if let Some(conn) = &state.conn {
            if !conn.client.is_closed() {
                return Ok(conn.client.clone());
            }
            tracing::warn!(server = %self.name, "connection closed, reconnecting");
            if let Some(conn) = state.conn.take() {
                tokio::spawn(stop(conn));
            }
            *self.tools.write().await = None;
        }
        if let Some((at, err)) = &state.last_failure
            && at.elapsed() < RETRY_BACKOFF
        {
            return Err(anyhow!("[{}] unavailable: {err}", self.name));
        }
        match tokio::time::timeout(self.startup_timeout, self.connect()).await {
            Ok(Ok(conn)) => {
                self.watch_tool_changes(&conn.client);
                if let Some(info) = conn.client.peer_info() {
                    *self.instructions.lock().expect("instructions lock") =
                        info.instructions.clone();
                }
                let client = conn.client.clone();
                state.conn = Some(conn);
                state.last_failure = None;
                tracing::info!(server = %self.name, "connected");
                Ok(client)
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                tracing::error!(server = %self.name, error = %msg, "connect failed");
                state.last_failure = Some((Instant::now(), msg.clone()));
                Err(anyhow!("[{}] connect failed: {msg}", self.name))
            }
            Err(_) => {
                let msg = format!("startup timed out after {:?}", self.startup_timeout);
                tracing::error!(server = %self.name, "{msg}");
                state.last_failure = Some((Instant::now(), msg.clone()));
                Err(anyhow!("[{}] {msg}", self.name))
            }
        }
    }

    async fn connect(&self) -> Result<Connection> {
        let handler = UpstreamHandler {
            tools: self.tools.clone(),
            tools_changed: self.tools_changed.clone(),
        };
        let transport = self
            .cfg
            .transport(&self.base_dir)
            .await
            .context("resolving config values")?;
        match transport {
            Transport::Stdio {
                command,
                args,
                env,
                inherit_env,
                cwd,
            } => {
                let workdir = cwd.clone().unwrap_or_else(|| self.base_dir.clone());
                let path_override = env
                    .iter()
                    .find(|(k, _)| is_path_var(k))
                    .map(|(_, v)| std::ffi::OsString::from(v));
                let program =
                    platform::resolve_program(&command, path_override.as_deref(), &workdir)?;
                let mut cmd = tokio::process::Command::new(&program);
                if !inherit_env {
                    cmd.env_clear().envs(Transport::base_env());
                }
                cmd.args(&args).envs(env);
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                let mut wrapped = CommandWrap::from(cmd);
                ProcessTree::isolate(&mut wrapped);
                let (transport, _) = TokioChildProcess::builder(wrapped)
                    .stderr(Stdio::inherit())
                    .spawn()
                    .with_context(|| format!("cannot spawn '{command}'"))?;
                let process_tree = transport.id().map(ProcessTree::from_pid);
                let client = match handler.serve(transport).await {
                    Ok(client) => client,
                    Err(e) => {
                        if let Some(tree) = process_tree {
                            tree.kill();
                        }
                        return Err(e.into());
                    }
                };
                Ok(Connection {
                    client: Arc::new(client),
                    process_tree,
                })
            }
            Transport::Http { url, headers } => {
                let mut custom = std::collections::HashMap::new();
                for (k, v) in headers {
                    custom.insert(
                        HeaderName::try_from(k.as_str())
                            .with_context(|| format!("invalid header name '{k}'"))?,
                        HeaderValue::try_from(v)
                            .with_context(|| format!("invalid value for header '{k}'"))?,
                    );
                }
                let config =
                    StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom);
                let transport = StreamableHttpClientTransport::from_config(config);
                Ok(Connection {
                    client: Arc::new(handler.serve(transport).await?),
                    process_tree: None,
                })
            }
        }
    }

    /// Servers speaking 2026-07-28+ only deliver list-changed notifications on an
    /// explicit subscription; older ones push them to [`UpstreamHandler`].
    fn watch_tool_changes(&self, client: &Arc<Client>) {
        let Some(info) = client.peer_info() else {
            return;
        };
        let supports_list_changed = info
            .capabilities
            .tools
            .as_ref()
            .and_then(|t| t.list_changed)
            .unwrap_or(false);
        if info.protocol_version < ProtocolVersion::V_2026_07_28 || !supports_list_changed {
            return;
        }
        let peer = client.peer().clone();
        let tools = self.tools.clone();
        let tools_changed = self.tools_changed.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            let filter = SubscriptionFilter::builder().tools_list_changed().build();
            let mut sub = match peer.listen(filter).await {
                Ok(sub) => sub,
                Err(e) => {
                    tracing::debug!(server = %name, error = %e, "no list-changed subscription");
                    return;
                }
            };
            while let Ok(Some(n)) = sub.next().await {
                if matches!(n, ServerNotification::ToolListChangedNotification(_)) {
                    *tools.write().await = None;
                    let _ = tools_changed.send(());
                }
            }
        });
    }
}

struct BusyGuard<'a>(&'a Upstream);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        *self.0.last_used.lock().expect("last_used lock") = Instant::now();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Closes the session (stdin EOF), then ends whatever is left of the process tree.
async fn stop(conn: Connection) {
    let Connection {
        client,
        process_tree,
    } = conn;
    match Arc::try_unwrap(client) {
        Ok(client) => {
            let _ = tokio::time::timeout(STOP_GRACE, client.cancel()).await;
        }
        Err(shared) => shared.cancellation_token().cancel(),
    }
    if let Some(tree) = process_tree {
        tree.terminate(STOP_GRACE).await;
    }
}

/// `PATH` from a server's `env` decides where its command is looked up.
fn is_path_var(key: &str) -> bool {
    if cfg!(windows) {
        key.eq_ignore_ascii_case("PATH")
    } else {
        key == "PATH"
    }
}

#[derive(Clone)]
pub struct UpstreamHandler {
    tools: ToolCache,
    tools_changed: broadcast::Sender<()>,
}

impl ClientHandler for UpstreamHandler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("emcipi", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        *self.tools.write().await = None;
        let _ = self.tools_changed.send(());
    }
}
