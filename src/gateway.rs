use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::future::join_all;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        Tool,
    },
    service::{NotificationContext, RequestContext},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::{
    cache::ToolStore,
    config::{Config, HttpConfig, Mode, Overrides},
    search,
    upstream::{Upstream, UpstreamOptions},
};

const SEARCH_TOOLS: &str = "search_tools";
const DESCRIBE_TOOLS: &str = "describe_tools";
const CALL_TOOL: &str = "call_tool";

const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 50;
const MAX_DESCRIBE: usize = 20;
const SUMMARY_CHARS: usize = 200;
const SERVER_DESCRIPTION_CHARS: usize = 250;
const SERVER_LIST_CHARS: usize = 4096;
const SUGGESTIONS: usize = 3;

/// Which servers search and describe look at.
#[derive(Clone, Copy)]
pub enum Scope {
    /// Servers in dynamic mode, given the session's default mode.
    Dynamic(Mode),
    /// Every server (used by the `search` CLI command).
    AllServers,
}

impl Scope {
    fn includes(self, upstream: &Upstream) -> bool {
        match self {
            Scope::Dynamic(default) => upstream.cfg.mode(default) == Mode::Dynamic,
            Scope::AllServers => true,
        }
    }
}

/// Upstream connections shared by all client sessions. The [`State`] is replaced on
/// config reload; requests keep working on the snapshot they started with.
pub struct Hub {
    state: std::sync::RwLock<Arc<State>>,
    peers: Mutex<Vec<Peer<RoleServer>>>,
    store: ToolStore,
    tools_changed: broadcast::Sender<()>,
    overrides: Overrides,
}

/// One version of the configuration with its upstreams.
pub struct State {
    pub default_mode: Mode,
    separator: String,
    tool_timeout: Duration,
    upstreams: Vec<Arc<Upstream>>,
}

impl Hub {
    pub fn new(mut cfg: Config, overrides: Overrides) -> Arc<Self> {
        overrides.apply(&mut cfg);
        let store = ToolStore::new(crate::platform::default_cache_dir());
        let (tools_changed, mut changes) = broadcast::channel(16);
        let (state, _) = State::build(cfg, None, &store, &tools_changed);
        let hub = Arc::new(Self {
            state: std::sync::RwLock::new(Arc::new(state)),
            peers: Mutex::default(),
            store,
            tools_changed,
            overrides,
        });

        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = changes.recv().await {
                let Some(hub) = weak.upgrade() else { break };
                hub.notify_tools_changed().await;
            }
        });

        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            loop {
                let Some(hub) = weak.upgrade() else { break };
                let state = hub.state();
                drop(hub);
                join_all(state.upstreams.iter().map(|u| u.stop_if_idle())).await;
                let shortest_idle = state
                    .upstreams
                    .iter()
                    .filter_map(|u| u.idle_timeout())
                    .min();
                let period = shortest_idle.map_or(Duration::from_secs(30), |idle| {
                    (idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30))
                });
                drop(state);
                tokio::time::sleep(period).await;
            }
        });
        hub
    }

    /// The current configuration snapshot.
    pub fn state(&self) -> Arc<State> {
        self.state.read().expect("state lock").clone()
    }

    /// Learns the tools of every upstream without a cached list, in the background, so
    /// the first `tools/list` is fast. Upstreams with a cached list are not started.
    pub fn warm_up(&self) {
        warm_up(&self.state().upstreams);
    }

    pub async fn shutdown(&self) {
        join_all(self.state().upstreams.iter().map(|u| u.shutdown())).await;
    }

    /// Applies a new configuration: unchanged servers keep running, added ones are
    /// created, removed and changed ones are stopped once their running calls finish.
    pub async fn reload(&self, mut cfg: Config) {
        self.overrides.apply(&mut cfg);
        let previous = self.state();
        let (state, retired) = State::build(cfg, Some(&previous), &self.store, &self.tools_changed);
        let added: Vec<Arc<Upstream>> = state
            .upstreams
            .iter()
            .filter(|u| !previous.upstreams.iter().any(|p| Arc::ptr_eq(p, u)))
            .cloned()
            .collect();
        tracing::info!(
            servers = state.upstreams.len(),
            started = added.len(),
            stopped = retired.len(),
            "config reloaded"
        );
        let grace = previous.tool_timeout;
        *self.state.write().expect("state lock") = Arc::new(state);
        drop(previous);
        warm_up(&added);
        for upstream in retired {
            tokio::spawn(async move {
                upstream.wait_idle(grace).await;
                upstream.shutdown().await;
            });
        }
        self.notify_tools_changed().await;
    }

    /// Reloads the config when the file changes (checked every 2 s) and, on Unix, on SIGHUP.
    /// Settings of the running HTTP listener cannot change; they only produce a warning.
    pub fn watch_config(self: &Arc<Self>, path: PathBuf, http: Option<HttpConfig>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut reload_signal = crate::platform::ReloadSignal::new();
            let mut last = file_stamp(&path);
            loop {
                let forced = tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => false,
                    _ = reload_signal.recv() => true,
                };
                if !forced && file_stamp(&path) == last {
                    continue;
                }
                // Editors often write in several steps; let the file settle.
                tokio::time::sleep(Duration::from_millis(200)).await;
                last = file_stamp(&path);
                let Some(hub) = weak.upgrade() else { break };
                match Config::load(&path) {
                    Ok(cfg) => {
                        if http.as_ref().is_some_and(|h| *h != cfg.http) {
                            tracing::warn!("[http] settings changed; restart emcipi to apply them");
                        }
                        hub.reload(cfg).await;
                    }
                    Err(e) => {
                        tracing::error!("config reload failed, keeping the current config: {e:#}")
                    }
                }
            }
        });
    }

    async fn notify_tools_changed(&self) {
        let mut peers = self.peers.lock().await;
        peers.retain(|p| !p.is_transport_closed());
        for peer in peers.iter() {
            let _ = peer.notify_tool_list_changed().await;
        }
    }
}

fn warm_up(upstreams: &[Arc<Upstream>]) {
    for upstream in upstreams.iter().filter(|u| !u.cached_at_start()) {
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let _ = upstream.tools().await;
        });
    }
}

fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl State {
    /// Builds the upstreams for `cfg`, reusing those of `previous` whose setup did not
    /// change. Returns the new state and the previous upstreams that are no longer used.
    fn build(
        cfg: Config,
        previous: Option<&State>,
        store: &ToolStore,
        tools_changed: &broadcast::Sender<()>,
    ) -> (Self, Vec<Arc<Upstream>>) {
        let startup_timeout = cfg.startup_timeout();
        let tool_timeout = cfg.tool_timeout();
        let upstreams: Vec<Arc<Upstream>> = cfg
            .servers
            .into_iter()
            .filter(|(_, s)| s.enabled)
            .map(|(name, s)| {
                let opts = UpstreamOptions {
                    base_dir: cfg.base_dir.clone(),
                    startup_timeout,
                    idle_timeout: s.idle_timeout(cfg.idle_timeout_secs),
                    tools_changed: tools_changed.clone(),
                    store: store.clone(),
                };
                let reused = previous
                    .and_then(|p| p.upstream(&name))
                    .filter(|u| u.has_setup(&s, &opts));
                match reused {
                    Some(u) => u.clone(),
                    None => Arc::new(Upstream::new(name, s, opts)),
                }
            })
            .collect();
        let retired = previous
            .map(|p| {
                p.upstreams
                    .iter()
                    .filter(|old| !upstreams.iter().any(|u| Arc::ptr_eq(u, old)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let state = Self {
            default_mode: cfg.mode,
            separator: cfg.separator,
            tool_timeout,
            upstreams,
        };
        (state, retired)
    }

    pub fn upstreams(&self) -> &[Arc<Upstream>] {
        &self.upstreams
    }

    pub fn qualified_name(&self, server: &str, tool: &str) -> String {
        format!("{server}{}{tool}", self.separator)
    }

    fn has_dynamic(&self, default: Mode) -> bool {
        self.upstreams
            .iter()
            .any(|u| Scope::Dynamic(default).includes(u))
    }

    fn upstream(&self, name: &str) -> Option<&Arc<Upstream>> {
        self.upstreams.iter().find(|u| u.name == name)
    }

    /// Tools of the selected upstreams, with qualified names. Failing upstreams are skipped.
    async fn collect_tools(
        &self,
        include: impl Fn(&Upstream) -> bool,
    ) -> Vec<(Arc<Upstream>, Tool)> {
        let selected = self.upstreams.iter().filter(|u| include(u));
        let results = join_all(selected.map(|u| async move { (u.clone(), u.tools().await) })).await;
        let mut out = Vec::new();
        for (upstream, result) in results {
            match result {
                Ok(tools) => {
                    for tool in tools.iter() {
                        let mut tool = tool.clone();
                        tool.name = self.qualified_name(&upstream.name, &tool.name).into();
                        out.push((upstream.clone(), tool));
                    }
                }
                Err(e) => tracing::warn!(server = %upstream.name, "skipping: {e:#}"),
            }
        }
        out
    }

    async fn route(&self, mut params: CallToolRequestParams) -> Result<CallToolResponse, McpError> {
        let qualified = params.name.to_string();
        let Some((server, tool)) = qualified.split_once(&self.separator) else {
            return Err(McpError::invalid_params(
                format!("unknown tool '{qualified}'{}", self.server_hint()),
                None,
            ));
        };
        let Some(upstream) = self.upstream(server) else {
            return Err(McpError::invalid_params(
                format!("unknown server '{server}'{}", self.server_hint()),
                None,
            ));
        };
        // Unknown names get suggestions instead of a round trip. The list is cached across
        // idle stops, so a typo does not start a stopped server.
        if let Ok(tools) = upstream.tools().await
            && !tools.iter().any(|t| t.name == tool)
        {
            let names: Vec<String> = tools
                .iter()
                .map(|t| self.qualified_name(server, &t.name))
                .collect();
            let pairs: Vec<(&str, &str)> = names
                .iter()
                .zip(tools.iter())
                .map(|(full, t)| (full.as_str(), t.name.as_ref()))
                .collect();
            return Ok(tool_error(format!(
                "unknown tool '{qualified}'.{}",
                did_you_mean(&search::closest(&qualified, &pairs, SUGGESTIONS))
            )));
        }
        params.name = tool.to_owned().into();
        // The downstream request metadata (protocol version, progress token, ...) belongs
        // to the client<->gateway connection; rmcp attaches its own for the upstream hop.
        params.meta = None;
        match tokio::time::timeout(self.tool_timeout, upstream.call(params)).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(e)) => Ok(tool_error(format!("{qualified}: {e:#}"))),
            Err(_) => Ok(tool_error(format!(
                "{qualified}: timed out after {:?}",
                self.tool_timeout
            ))),
        }
    }

    fn server_hint(&self) -> String {
        let names: Vec<&str> = self.upstreams.iter().map(|u| u.name.as_str()).collect();
        format!(
            "; tool names have the form <server>{}<tool>, servers: {}",
            self.separator,
            names.join(", ")
        )
    }

    /// Text result of `search_tools`; also used by the `search` CLI command.
    pub async fn search(&self, args: SearchArgs, scope: Scope) -> String {
        if let Some(server) = &args.server
            && !self
                .upstreams
                .iter()
                .any(|u| &u.name == server && scope.includes(u))
        {
            return format!(
                "Unknown server '{server}'.\n\n{}",
                self.server_overview(scope, &[])
            );
        }
        let tools = self
            .collect_tools(|u| {
                scope.includes(u) && args.server.as_ref().is_none_or(|s| *s == u.name)
            })
            .await;
        let query = args.query.trim();
        if query.is_empty() && args.server.is_none() {
            return format!(
                "{}\nSearch with keywords, or pass `server` with an empty query to list its tools.",
                self.server_overview(scope, &tools)
            );
        }

        let namespaces: Vec<String> = tools
            .iter()
            .map(|(u, _)| {
                let desc = u.cfg.description.as_deref().unwrap_or("");
                let instructions = u.instructions().unwrap_or_default();
                format!("{} {desc} {instructions}", u.name)
            })
            .collect();
        let local_names: Vec<&str> = tools
            .iter()
            .map(|(u, t)| &t.name[u.name.len() + self.separator.len()..])
            .collect();

        let hits: Vec<usize> = if query.is_empty() {
            let mut all: Vec<usize> = (0..tools.len()).collect();
            all.sort_by(|&a, &b| tools[a].1.name.cmp(&tools[b].1.name));
            all
        } else {
            let docs: Vec<search::Document<'_>> = tools
                .iter()
                .enumerate()
                .map(|(i, (_, tool))| search::Document {
                    name: &tool.name,
                    local_name: local_names[i],
                    tool,
                    namespace: &namespaces[i],
                })
                .collect();
            search::rank(query, &docs)
                .into_iter()
                .map(|h| h.index)
                .collect()
        };

        if hits.is_empty() {
            let pairs: Vec<(&str, &str)> = tools
                .iter()
                .zip(&local_names)
                .map(|((_, t), local)| (t.name.as_ref(), *local))
                .collect();
            let suggestions = search::closest(query, &pairs, SUGGESTIONS);
            return format!(
                "No tools match '{query}'.{}\n\n{}\nTry other keywords, or pass `server` with an empty query to list its tools.",
                did_you_mean(&suggestions),
                self.server_overview(scope, &tools)
            );
        }

        let default_limit = if query.is_empty() {
            MAX_SEARCH_LIMIT
        } else {
            DEFAULT_SEARCH_LIMIT
        };
        let limit = args
            .limit
            .unwrap_or(default_limit)
            .clamp(1, MAX_SEARCH_LIMIT);
        let total = hits.len();
        let shown = &hits[..total.min(limit)];

        if args.include_schema {
            let tools: Vec<Value> = shown.iter().map(|&i| describe(&tools[i].1)).collect();
            return json!({ "total": total, "tools": tools }).to_string();
        }

        let mut out = format!("{} of {total} matching tools.\n", shown.len());
        let mut servers_seen: Vec<&str> = Vec::new();
        for &i in shown {
            let upstream = &tools[i].0;
            if !servers_seen.contains(&upstream.name.as_str()) {
                servers_seen.push(&upstream.name);
            }
        }
        let described: Vec<String> = servers_seen
            .iter()
            .filter_map(|name| {
                let u = self.upstream(name)?;
                let desc = u.cfg.description.as_deref()?;
                Some(format!(
                    "{name}: {}",
                    search::summary(desc, SERVER_DESCRIPTION_CHARS)
                ))
            })
            .collect();
        if !described.is_empty() {
            let _ = writeln!(out, "Servers: {}", described.join(" | "));
        }
        out.push('\n');
        for &i in shown {
            let tool = &tools[i].1;
            let summary = search::summary(tool.description.as_deref().unwrap_or(""), SUMMARY_CHARS);
            let _ = writeln!(
                out,
                "- {}: {summary}",
                search::signature(&tool.name, &tool.input_schema)
            );
        }
        let _ = write!(
            out,
            "\nParameters marked `?` are optional. Call `{DESCRIBE_TOOLS}` for full input schemas, then `{CALL_TOOL}`."
        );
        out
    }

    /// `- server: description (N tools)` for every server in scope.
    fn server_overview(&self, scope: Scope, tools: &[(Arc<Upstream>, Tool)]) -> String {
        let mut out = String::from("Servers:\n");
        for u in self.upstreams.iter().filter(|u| scope.includes(u)) {
            let count = tools.iter().filter(|(t, _)| t.name == u.name).count();
            let _ = write!(out, "- {}", u.name);
            if let Some(desc) = &u.cfg.description {
                let _ = write!(out, ": {}", search::summary(desc, SERVER_DESCRIPTION_CHARS));
            }
            if count > 0 {
                let _ = write!(out, " ({count} tools)");
            }
            out.push('\n');
        }
        out
    }

    async fn describe_tools(&self, args: DescribeArgs) -> String {
        let all = self.collect_tools(|_| true).await;
        let requested = args.names.len();
        let mut found = Vec::new();
        let mut unknown = Vec::new();
        for name in args.names.iter().take(MAX_DESCRIBE) {
            match all.iter().find(|(_, t)| t.name == *name) {
                Some((_, tool)) => found.push(describe(tool)),
                None => {
                    let locals: Vec<&str> = all
                        .iter()
                        .map(|(u, t)| &t.name[u.name.len() + self.separator.len()..])
                        .collect();
                    let pairs: Vec<(&str, &str)> = all
                        .iter()
                        .zip(&locals)
                        .map(|((_, t), local)| (t.name.as_ref(), *local))
                        .collect();
                    let suggestions = search::closest(name, &pairs, SUGGESTIONS);
                    unknown.push(json!({ "name": name, "did_you_mean": suggestions }));
                }
            }
        }
        let mut result = json!({ "tools": found });
        if !unknown.is_empty() {
            result["unknown"] = json!(unknown);
        }
        if requested > MAX_DESCRIBE {
            result["note"] = json!(format!(
                "only the first {MAX_DESCRIBE} of {requested} names were described"
            ));
        }
        result.to_string()
    }

    async fn call_via_meta(
        &self,
        outer: CallToolRequestParams,
        args: CallArgs,
    ) -> Result<CallToolResponse, McpError> {
        let mut params = CallToolRequestParams::new(args.name);
        params.arguments = args.arguments;
        params.input_responses = outer.input_responses;
        params.request_state = outer.request_state;
        self.route(params).await
    }

    /// Dynamic-mode servers with their description, capped per server and in total.
    fn server_list(&self, default: Mode) -> String {
        let mut out = String::new();
        for u in self
            .upstreams
            .iter()
            .filter(|u| Scope::Dynamic(default).includes(u))
        {
            let line = match &u.cfg.description {
                Some(d) => format!(
                    "\n- {}: {}",
                    u.name,
                    search::summary(d, SERVER_DESCRIPTION_CHARS)
                ),
                None => format!("\n- {}", u.name),
            };
            if out.len() + line.len() > SERVER_LIST_CHARS {
                out.push_str("\n- …");
                break;
            }
            out.push_str(&line);
        }
        out
    }

    fn instructions(&self, default: Mode) -> String {
        let topics: Vec<String> = self
            .upstreams
            .iter()
            .filter(|u| Scope::Dynamic(default).includes(u))
            .map(|u| match &u.cfg.description {
                Some(d) => format!("{} ({})", u.name, search::summary(d, 80)),
                None => u.name.clone(),
            })
            .collect();
        format!(
            "This gateway connects tools for: {}. They are not listed directly: find them with \
             `{SEARCH_TOOLS}` using keywords such as \"create issue\", check input schemas with \
             `{DESCRIBE_TOOLS}` if unsure, and run them with `{CALL_TOOL}`.",
            topics.join("; ")
        )
    }

    fn meta_tools(&self, default: Mode) -> Vec<Tool> {
        let servers = self.server_list(default);
        vec![
            Tool::new(
                SEARCH_TOOLS,
                format!(
                    "Search the tools of the connected MCP servers. Most tools of this gateway are not listed directly; \
                     use this tool to find them. Ranks tools with BM25 over tool names, descriptions and parameter names. \
                     Use a few keywords for the action and the object, e.g. \"create issue\", \"list pull requests\", \
                     \"read file\". An exact tool name ranks that tool first. Returns tool names with their parameters \
                     and a one-line description. An empty query with `server` lists all tools of that server. Servers:{servers}"
                ),
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Keywords for what you want to do, e.g. 'create issue' or 'search code'. Or an exact tool name." },
                        "server": { "type": "string", "description": "Only search the tools of this server." },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_LIMIT, "description": "Maximum number of results (default 8)." },
                        "include_schema": { "type": "boolean", "description": "Return full input schemas instead of one-line signatures." }
                    },
                    "required": ["query"]
                })),
            ),
            Tool::new(
                DESCRIBE_TOOLS,
                "Get the full definitions (description and input schema) of tools by their exact names from search_tools. \
                 Unknown names are reported with the closest matching names.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "names": { "type": "array", "items": { "type": "string" }, "maxItems": MAX_DESCRIBE, "description": "Exact tool names as returned by search_tools." }
                    },
                    "required": ["names"]
                })),
            ),
            Tool::new(
                CALL_TOOL,
                "Run a tool found via search_tools. Pass its exact name and arguments matching its input schema; \
                 if you are unsure about the arguments, check them with describe_tools first.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Exact tool name as returned by search_tools." },
                        "arguments": { "type": "object", "description": "Arguments matching the tool's input schema." }
                    },
                    "required": ["name"]
                })),
            ),
        ]
    }
}

/// One client session. `mode` forces a default mode for servers without a pinned one;
/// `None` follows the config.
#[derive(Clone)]
pub struct Gateway {
    pub hub: Arc<Hub>,
    pub mode: Option<Mode>,
}

impl Gateway {
    fn mode(&self, state: &State) -> Mode {
        self.mode.unwrap_or(state.default_mode)
    }
}

impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerConfig {
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        let info = ServerConfig::new(caps)
            .with_server_info(Implementation::new("emcipi", env!("CARGO_PKG_VERSION")));
        let state = self.hub.state();
        let mode = self.mode(&state);
        if state.has_dynamic(mode) {
            info.with_instructions(state.instructions(mode))
        } else {
            info
        }
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.hub.peers.lock().await.push(context.peer);
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let state = self.hub.state();
        let mode = self.mode(&state);
        let mut tools: Vec<Tool> = state
            .collect_tools(|u| u.cfg.mode(mode) == Mode::Static)
            .await
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        if state.has_dynamic(mode) {
            tools.extend(state.meta_tools(mode));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let state = self.hub.state();
        let mode = self.mode(&state);
        if state.has_dynamic(mode) {
            match request.name.as_ref() {
                SEARCH_TOOLS => {
                    let args = parse_args(&request)?;
                    return Ok(tool_text(state.search(args, Scope::Dynamic(mode)).await));
                }
                DESCRIBE_TOOLS => {
                    return Ok(tool_text(state.describe_tools(parse_args(&request)?).await));
                }
                CALL_TOOL => {
                    let args = parse_args(&request)?;
                    return state.call_via_meta(request, args).await;
                }
                _ => {}
            }
        }
        state.route(request).await
    }
}

#[derive(Deserialize)]
pub struct SearchArgs {
    #[serde(default)]
    pub query: String,
    pub server: Option<String>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub include_schema: bool,
}

#[derive(Deserialize)]
struct DescribeArgs {
    names: Vec<String>,
}

#[derive(Deserialize)]
struct CallArgs {
    name: String,
    arguments: Option<JsonObject>,
}

fn parse_args<T: serde::de::DeserializeOwned>(req: &CallToolRequestParams) -> Result<T, McpError> {
    let args = Value::Object(req.arguments.clone().unwrap_or_default());
    serde_json::from_value(args).map_err(|e| McpError::invalid_params(e.to_string(), None))
}

fn schema(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("schemas are object literals"),
    }
}

fn describe(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.input_schema,
    })
}

fn did_you_mean(names: &[&str]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!(" Did you mean: {}?", names.join(", "))
    }
}

fn tool_text(text: String) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(text)]).into()
}

fn tool_error(text: String) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(text)]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn reload_keeps_unchanged_upstreams() {
        let store = ToolStore::new(None);
        let (tx, _) = broadcast::channel(1);
        let before = config(
            r#"
            [servers.same]
            command = "a"
            [servers.changed]
            command = "b"
            [servers.removed]
            command = "c"
            "#,
        );
        let after = config(
            r#"
            [servers.same]
            command = "a"
            [servers.changed]
            command = "b"
            args = ["--new"]
            [servers.added]
            command = "d"
            "#,
        );
        let (old, _) = State::build(before, None, &store, &tx);
        let (new, retired) = State::build(after, Some(&old), &store, &tx);

        let same = |s: &State| s.upstream("same").unwrap().clone();
        assert!(Arc::ptr_eq(&same(&old), &same(&new)));
        assert!(!Arc::ptr_eq(
            old.upstream("changed").unwrap(),
            new.upstream("changed").unwrap()
        ));
        let mut retired: Vec<&str> = retired.iter().map(|u| u.name.as_str()).collect();
        retired.sort();
        assert_eq!(retired, ["changed", "removed"]);
        assert!(new.upstream("added").is_some());
    }

    #[test]
    fn global_settings_that_affect_upstreams_restart_them() {
        let store = ToolStore::new(None);
        let (tx, _) = broadcast::channel(1);
        let (old, _) = State::build(config("[servers.x]\ncommand = \"a\""), None, &store, &tx);
        let with_idle = config("idle_timeout_secs = 60\n[servers.x]\ncommand = \"a\"");
        let (new, retired) = State::build(with_idle, Some(&old), &store, &tx);
        assert_eq!(retired.len(), 1);
        assert!(!Arc::ptr_eq(
            old.upstream("x").unwrap(),
            new.upstream("x").unwrap()
        ));
    }
}
