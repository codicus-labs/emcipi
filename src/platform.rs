//! Everything that differs between Unix (Linux, macOS) and Windows.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use process_wrap::tokio::CommandWrap;

/// Environment variables a stdio server sees unless `inherit_env = true`.
pub fn base_env_vars() -> impl Iterator<Item = &'static str> {
    PLATFORM_ENV.iter().chain(NETWORK_ENV).copied()
}

/// Network settings: without them `npx`/`uvx` cannot download packages behind a
/// corporate proxy or with a custom CA. A proxy URL can contain credentials, but it is
/// configuration the user already exposes to every program.
const NETWORK_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
];

#[cfg(unix)]
const PLATFORM_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TMPDIR",
    "TZ",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
];

/// Without `SystemRoot`, `PATHEXT` and the profile directories many programs
/// (including Node.js) fail to start on Windows.
#[cfg(windows)]
const PLATFORM_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME/emcipi/config.toml`, else `~/.config/emcipi/config.toml`.
#[cfg(unix)]
pub fn default_config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("emcipi")
        .join("config.toml")
}

/// `%APPDATA%\emcipi\config.toml`.
#[cfg(windows)]
pub fn default_config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("emcipi")
        .join("config.toml")
}

/// Where tool lists are cached: `$XDG_CACHE_HOME/emcipi` or `~/.cache/emcipi` on Linux,
/// `~/Library/Caches/emcipi` on macOS, `%LOCALAPPDATA%\emcipi\cache` on Windows.
pub fn default_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let dir = home_dir().map(|h| h.join("Library/Caches/emcipi"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))
        .map(|d| d.join("emcipi"));
    #[cfg(windows)]
    let dir = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("emcipi").join("cache"));
    dir
}

/// Where credentials are kept: `$XDG_STATE_HOME/emcipi` or `~/.local/state/emcipi` on
/// Linux, `~/Library/Application Support/emcipi` on macOS, `%LOCALAPPDATA%\emcipi` on Windows.
pub fn default_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let dir = home_dir().map(|h| h.join("Library/Application Support/emcipi"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let dir = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local/state")))
        .map(|d| d.join("emcipi"));
    #[cfg(windows)]
    let dir = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("emcipi"));
    dir
}

/// Opens `url` in the default browser; failures are ignored because the URL is also printed.
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    // `start` would need cmd.exe, which splits URLs at `&`.
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler");
        cmd
    };
    let _ = cmd
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Writes `data` so that only the current user can read it: mode 0600 on Unix; on
/// Windows the per-user profile directories are already private.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
}

/// `~/` (or `~\`) means the home directory; relative paths resolve against `base_dir`.
pub fn expand_path(path: &str, base_dir: &Path) -> PathBuf {
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));
    let expanded = match (rest, home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    };
    if expanded.is_relative() {
        base_dir.join(expanded)
    } else {
        expanded
    }
}

/// Resolves a bare program name via `PATH` (on Windows including `PATHEXT`, so `npx`
/// finds `npx.cmd`). Names containing a path separator are taken as they are.
pub fn resolve_program(program: &str, path_var: Option<&OsStr>, cwd: &Path) -> Result<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        return Ok(PathBuf::from(program));
    }
    let path_var = path_var
        .map(OsStr::to_os_string)
        .or_else(|| std::env::var_os("PATH"));
    which::which_in(program, path_var, cwd)
        .with_context(|| format!("program '{program}' not found in PATH"))
}

/// The process tree of a stdio server, so wrappers like `npx` or `uvx` do not leave
/// their children behind.
///
/// - Unix: the server leads its own process group. After the session is closed the
///   group gets SIGTERM, then SIGKILL.
/// - Windows: the server runs in a Job Object. rmcp's shutdown kills the child, which
///   terminates the whole job; the job is also terminated when emcipi exits.
#[derive(Clone, Copy, Debug)]
pub struct ProcessTree {
    #[cfg(unix)]
    pgid: i32,
}

#[cfg(unix)]
impl ProcessTree {
    pub fn isolate(cmd: &mut CommandWrap) {
        cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
    }

    pub fn from_pid(pid: u32) -> Self {
        Self { pgid: pid as i32 }
    }

    /// Ends whatever is left of the tree once the session has been closed.
    pub async fn terminate(self, grace: Duration) {
        use nix::sys::signal::Signal;
        if !self.alive() {
            return;
        }
        self.signal(Signal::SIGTERM);
        let deadline = std::time::Instant::now() + grace;
        while self.alive() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if self.alive() {
            tracing::warn!(pgid = self.pgid, "process group ignored SIGTERM, killing");
            self.signal(Signal::SIGKILL);
        }
    }

    /// Immediate kill, e.g. when the MCP handshake failed.
    pub fn kill(self) {
        self.signal(nix::sys::signal::Signal::SIGKILL);
    }

    fn signal(self, signal: nix::sys::signal::Signal) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pgid), signal);
    }

    fn alive(self) -> bool {
        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pgid), None).is_ok()
    }
}

#[cfg(windows)]
impl ProcessTree {
    pub fn isolate(cmd: &mut CommandWrap) {
        cmd.wrap(process_wrap::tokio::JobObject);
        cmd.wrap(process_wrap::tokio::KillOnDrop);
    }

    pub fn from_pid(_pid: u32) -> Self {
        Self {}
    }

    /// Nothing to do: closing the session already terminated the job.
    pub async fn terminate(self, _grace: Duration) {}

    /// Dropping the transport terminates the job.
    pub fn kill(self) {}
}

/// Requests a config reload from outside: `SIGHUP` on Unix, nothing on Windows.
pub struct ReloadSignal {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl ReloadSignal {
    pub fn new() -> Self {
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup());
        if let Err(e) = &signal {
            tracing::warn!("cannot listen for SIGHUP: {e}");
        }
        Self {
            signal: signal.ok(),
        }
    }

    pub async fn recv(&mut self) {
        match &mut self.signal {
            Some(signal) => {
                signal.recv().await;
            }
            None => std::future::pending().await,
        }
    }
}

#[cfg(windows)]
impl ReloadSignal {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn recv(&mut self) {
        std::future::pending::<()>().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_and_relative_paths() {
        let base = Path::new("/base");
        if let Some(home) = home_dir() {
            assert_eq!(expand_path("~/x", base), home.join("x"));
        }
        assert_eq!(
            expand_path("secrets/token", base),
            base.join("secrets/token")
        );
    }

    #[test]
    fn resolves_programs() {
        let cwd = std::env::current_dir().unwrap();
        assert!(resolve_program("rustc", None, &cwd).is_ok());
        assert!(resolve_program("emcipi-no-such-program", None, &cwd).is_err());
        assert_eq!(
            resolve_program("./local/bin", None, &cwd).unwrap(),
            PathBuf::from("./local/bin")
        );
    }
}
