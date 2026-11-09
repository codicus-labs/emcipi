use std::{fmt::Write as _, sync::Arc, time::Duration};

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
    config::{Config, Mode},
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

/// Upstream connections shared by all client sessions.
pub struct Hub {
    separator: String,
    tool_timeout: Duration,
    upstreams: Vec<Arc<Upstream>>,
    peers: Mutex<Vec<Peer<RoleServer>>>,
}

impl Hub {
    pub fn new(cfg: Config) -> Arc<Self> {
        let store = ToolStore::new(crate::platform::default_cache_dir());
        let (tools_changed, mut changes) = broadcast::channel(16);
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
                Arc::new(Upstream::new(name, s, opts))
            })
            .collect();
        let shortest_idle = upstreams.iter().filter_map(|u| u.idle_timeout()).min();
        let hub = Arc::new(Self {
            separator: cfg.separator,
            tool_timeout,
            upstreams,
            peers: Mutex::default(),
        });

        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = changes.recv().await {
                let Some(hub) = weak.upgrade() else { break };
                hub.notify_tools_changed().await;
            }
        });

        if let Some(idle) = shortest_idle {
            let weak = Arc::downgrade(&hub);
            let period = (idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30));
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(period);
                loop {
                    tick.tick().await;
                    let Some(hub) = weak.upgrade() else { break };
                    join_all(hub.upstreams.iter().map(|u| u.stop_if_idle())).await;
                }
            });
        }
        hub
    }

    pub fn upstreams(&self) -> &[Arc<Upstream>] {
        &self.upstreams
    }

    pub fn qualified_name(&self, server: &str, tool: &str) -> String {
        format!("{server}{}{tool}", self.separator)
    }

    /// Learns the tools of every upstream without a cached list, in the background, so
    /// the first `tools/list` is fast. Upstreams with a cached list are not started.
    pub fn warm_up(self: &Arc<Self>) {
        for upstream in self.upstreams.iter().filter(|u| !u.cached_at_start()) {
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let _ = upstream.tools().await;
            });
        }
    }

    pub async fn shutdown(&self) {
        join_all(self.upstreams.iter().map(|u| u.shutdown())).await;
    }

    fn has_dynamic(&self, default: Mode) -> bool {
        self.upstreams
            .iter()
            .any(|u| Scope::Dynamic(default).includes(u))
    }

    fn upstream(&self, name: &str) -> Option<&Arc<Upstream>> {
        self.upstreams.iter().find(|u| u.name == name)
    }

    async fn notify_tools_changed(&self) {
        let mut peers = self.peers.lock().await;
        peers.retain(|p| !p.is_transport_closed());
        for peer in peers.iter() {
            let _ = peer.notify_tool_list_changed().await;
        }
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

/// One client session. `mode` is the default for servers without a pinned mode.
#[derive(Clone)]
pub struct Gateway {
    pub hub: Arc<Hub>,
    pub mode: Mode,
}

impl ServerHandler for Gateway {
    fn get_info(&self) -> ServerConfig {
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        let info = ServerConfig::new(caps)
            .with_server_info(Implementation::new("emcipi", env!("CARGO_PKG_VERSION")));
        if self.hub.has_dynamic(self.mode) {
            info.with_instructions(self.hub.instructions(self.mode))
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
        let mode = self.mode;
        let mut tools: Vec<Tool> = self
            .hub
            .collect_tools(|u| u.cfg.mode(mode) == Mode::Static)
            .await
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        if self.hub.has_dynamic(mode) {
            tools.extend(self.hub.meta_tools(mode));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let hub = &self.hub;
        if hub.has_dynamic(self.mode) {
            match request.name.as_ref() {
                SEARCH_TOOLS => {
                    let args = parse_args(&request)?;
                    return Ok(tool_text(hub.search(args, Scope::Dynamic(self.mode)).await));
                }
                DESCRIBE_TOOLS => {
                    return Ok(tool_text(hub.describe_tools(parse_args(&request)?).await));
                }
                CALL_TOOL => {
                    let args = parse_args(&request)?;
                    return hub.call_via_meta(request, args).await;
                }
                _ => {}
            }
        }
        hub.route(request).await
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
