use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{interpolate, platform};

/// How the tools of an upstream server are offered to the client.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Every tool is listed directly as `<server><separator><tool>`.
    #[default]
    Static,
    /// Tools are hidden behind the `search_tools` / `describe_tools` / `call_tool` meta tools.
    Dynamic,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_separator")]
    pub separator: String,
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: u64,
    /// Stop stdio servers after this many seconds without a call. Unset or 0: never.
    pub idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
    /// Directory of the config file; relative paths in placeholders resolve against it.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_path")]
    pub path: String,
    /// `Host` header allowlist (DNS rebinding protection). Defaults to loopback names.
    pub allowed_hosts: Option<Vec<String>>,
    /// When set, every MCP request must carry `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            path: default_path(),
            allowed_hosts: None,
            auth_token: None,
        }
    }
}

/// Values of `command`, `args`, `env`, `cwd`, `url` and `headers` may contain
/// placeholders (see [`interpolate`]); they are resolved on every connect.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Short hint for the model what this server is good for (used in dynamic mode).
    pub description: Option<String>,
    /// Pins the mode of this server regardless of the gateway default.
    pub mode: Option<Mode>,
    /// Overrides the global `idle_timeout_secs`. 0 keeps the server running.
    pub idle_timeout_secs: Option<u64>,

    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// When true, the server sees the gateway's whole environment. By default it only
    /// sees [`platform::base_env_vars`] plus its own `env`, so secrets must be passed explicitly.
    #[serde(default)]
    pub inherit_env: bool,
    pub cwd: Option<String>,

    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// Glob patterns (`*` wildcard). Empty means all tools.
    #[serde(default)]
    pub include_tools: Vec<String>,
    #[serde(default)]
    pub exclude_tools: Vec<String>,
}

pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        inherit_env: bool,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl Transport {
    pub fn base_env() -> impl Iterator<Item = (String, String)> {
        platform::base_env_vars().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_owned(), v)))
    }
}

impl ServerConfig {
    pub fn is_stdio(&self) -> bool {
        self.command.is_some()
    }

    pub fn mode(&self, default: Mode) -> Mode {
        self.mode.unwrap_or(default)
    }

    pub fn idle_timeout(&self, global: Option<u64>) -> Option<Duration> {
        match self.idle_timeout_secs.or(global) {
            Some(0) | None => None,
            Some(secs) if self.is_stdio() => Some(Duration::from_secs(secs)),
            Some(_) => None,
        }
    }

    /// Resolves all placeholders. Runs `${cmd:...}` and reads `${file:...}`.
    pub async fn transport(&self, base_dir: &Path) -> Result<Transport> {
        if let Some(command) = &self.command {
            let mut args = Vec::with_capacity(self.args.len());
            for arg in &self.args {
                args.push(interpolate::resolve(arg, base_dir).await?);
            }
            let mut env = Vec::with_capacity(self.env.len());
            for (key, value) in &self.env {
                env.push((
                    key.clone(),
                    interpolate::resolve(value, base_dir)
                        .await
                        .with_context(|| format!("env.{key}"))?,
                ));
            }
            let cwd = match &self.cwd {
                Some(cwd) => Some(platform::expand_path(
                    &interpolate::resolve(cwd, base_dir).await?,
                    base_dir,
                )),
                None => None,
            };
            return Ok(Transport::Stdio {
                command: interpolate::resolve(command, base_dir).await?,
                args,
                env,
                inherit_env: self.inherit_env,
                cwd,
            });
        }
        let url = self.url.as_deref().expect("validated in Config::load");
        let mut headers = Vec::with_capacity(self.headers.len());
        for (key, value) in &self.headers {
            headers.push((
                key.clone(),
                interpolate::resolve(value, base_dir)
                    .await
                    .with_context(|| format!("headers.{key}"))?,
            ));
        }
        Ok(Transport::Http {
            url: interpolate::resolve(url, base_dir).await?,
            headers,
        })
    }

    /// Identifies what the server is, as written in the config (placeholders unresolved),
    /// so cached tool lists are dropped when it changes. Filters and modes are excluded.
    pub fn fingerprint(&self) -> String {
        let identity = serde_json::json!({
            "command": self.command,
            "args": self.args,
            "env": self.env,
            "cwd": self.cwd,
            "url": self.url,
            "headers": self.headers,
        });
        crate::cache::fingerprint(identity.to_string().as_bytes())
    }

    pub fn tool_allowed(&self, tool: &str) -> bool {
        let included =
            self.include_tools.is_empty() || self.include_tools.iter().any(|p| glob_match(p, tool));
        included && !self.exclude_tools.iter().any(|p| glob_match(p, tool))
    }

    fn templates(&self) -> impl Iterator<Item = (String, &str)> {
        let single = [
            ("command", self.command.as_deref()),
            ("cwd", self.cwd.as_deref()),
            ("url", self.url.as_deref()),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v)));
        let args = self.args.iter().map(|a| ("args".to_owned(), a.as_str()));
        let env = self
            .env
            .iter()
            .map(|(k, v)| (format!("env.{k}"), v.as_str()));
        let headers = self
            .headers
            .iter()
            .map(|(k, v)| (format!("headers.{k}"), v.as_str()));
        single.chain(args).chain(env).chain(headers)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("invalid config {}", path.display()))?;
        cfg.base_dir = path
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn default_path() -> PathBuf {
        platform::default_config_path()
    }

    pub fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.startup_timeout_secs)
    }

    pub fn tool_timeout(&self) -> Duration {
        Duration::from_secs(self.tool_timeout_secs)
    }

    fn validate(&self) -> Result<()> {
        if self.separator.is_empty() {
            bail!("separator must not be empty");
        }
        if !self.http.path.starts_with('/') {
            bail!("http.path must start with '/'");
        }
        if let Some(token) = &self.http.auth_token {
            interpolate::check(token).context("http.auth_token")?;
        }
        for (name, server) in &self.servers {
            let valid_chars = name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if name.is_empty() || !valid_chars {
                bail!("server name '{name}' may only contain [A-Za-z0-9_-]");
            }
            if name.contains(&self.separator) {
                bail!(
                    "server name '{name}' must not contain the separator '{}'",
                    self.separator
                );
            }
            match (&server.command, &server.url) {
                (Some(_), Some(_)) => {
                    bail!("servers.{name}: set either 'command' or 'url', not both")
                }
                (None, None) => bail!("servers.{name}: needs 'command' (stdio) or 'url' (http)"),
                _ => {}
            }
            for (field, value) in server.templates() {
                interpolate::check(value).with_context(|| format!("servers.{name}.{field}"))?;
            }
        }
        Ok(())
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let Some((first, rest)) = pattern.split_once('*') else {
        return pattern == text;
    };
    let Some(mut remaining) = text.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = rest.split('*').collect();
    let last = parts.pop().unwrap_or_default();
    for part in parts {
        match remaining.find(part) {
            Some(i) => remaining = &remaining[i + part.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
}

fn default_separator() -> String {
    "__".into()
}
fn default_startup_timeout() -> u64 {
    30
}
fn default_tool_timeout() -> u64 {
    300
}
fn default_listen() -> String {
    "127.0.0.1:8808".into()
}
fn default_path() -> String {
    "/mcp".into()
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("get_*", "get_issue"));
        assert!(!glob_match("get_*", "list_issues"));
        assert!(glob_match("*_issue", "create_issue"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxc"));
        assert!(!glob_match("ab*ab", "ab"));
        assert!(glob_match("exact", "exact"));
    }

    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.mode, Mode::Static);
        assert_eq!(cfg.servers["github"].mode, Some(Mode::Dynamic));
        assert!(cfg.servers.len() >= 4);
    }

    #[test]
    fn bad_placeholder_fails_validation() {
        let cfg: Config = toml::from_str(
            r#"
            [servers.x]
            command = "x"
            env = { TOKEN = "${cmd:" }
            "#,
        )
        .unwrap();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("servers.x.env.TOKEN"), "{err}");
    }

    #[test]
    fn idle_timeout_only_for_stdio() {
        let stdio: ServerConfig = toml::from_str("command = \"x\"").unwrap();
        let http: ServerConfig = toml::from_str("url = \"http://x\"").unwrap();
        let pinned: ServerConfig =
            toml::from_str("command = \"x\"\nidle_timeout_secs = 0").unwrap();
        assert_eq!(stdio.idle_timeout(Some(60)), Some(Duration::from_secs(60)));
        assert_eq!(stdio.idle_timeout(None), None);
        assert_eq!(http.idle_timeout(Some(60)), None);
        assert_eq!(pinned.idle_timeout(Some(60)), None);
    }

    #[test]
    fn defaults_are_static_and_isolated() {
        let cfg: Config = toml::from_str("[servers.x]\ncommand = \"x\"").unwrap();
        assert_eq!(cfg.mode, Mode::Static);
        assert!(!cfg.servers["x"].inherit_env);
        let pinned: ServerConfig = toml::from_str("command = \"x\"\nmode = \"static\"").unwrap();
        assert_eq!(pinned.mode(Mode::Dynamic), Mode::Static);
        assert!(toml::from_str::<ServerConfig>("command = \"x\"\nmode = \"all\"").is_err());
    }

    #[test]
    fn tool_filter() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            command = "x"
            include_tools = ["get_*", "list_*"]
            exclude_tools = ["*secret*"]
            "#,
        )
        .unwrap();
        assert!(cfg.tool_allowed("get_issue"));
        assert!(!cfg.tool_allowed("delete_issue"));
        assert!(!cfg.tool_allowed("get_secret_value"));
    }
}
