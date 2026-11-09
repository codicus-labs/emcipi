# emcipi

A lean MCP gateway in Rust for Linux, macOS and Windows. It puts any number of MCP servers (stdio or Streamable HTTP) behind a single endpoint and is configured with a TOML file. Uses about 8 MB of RAM, not counting the servers it runs.

## Installation

Binaries for Linux, macOS and Windows are on the [releases](../../releases) page. To build from source: `cargo build --release`.

> **macOS:** The binaries are not signed. After a browser download, run `xattr -d com.apple.quarantine emcipi`.

## Usage

```sh
emcipi http                    # Streamable HTTP on 127.0.0.1:8808/mcp, one process for all clients
emcipi stdio                   # one gateway per client
emcipi tools                   # test the config: connect all servers, list their tools
emcipi search "create issue"   # test the tool search
```

| Flag | Env | |
|---|---|---|
| `-c, --config` | `EMCIPI_CONFIG` | default `~/.config/emcipi/config.toml`, Windows `%APPDATA%\emcipi\config.toml` |
| `-m, --mode static\|dynamic` | `EMCIPI_MODE` | default mode (default `static`) |
| `--idle-timeout SECS` | `EMCIPI_IDLE_TIMEOUT` | stop unused stdio servers, 0 = never |
| `http --listen ADDR` | `EMCIPI_LISTEN` | |
| — | `EMCIPI_AUTH_TOKEN` | bearer token required for inbound HTTP requests |

Logs go to stderr; set the level with `RUST_LOG`.

### Connecting clients

With `emcipi http`, all clients share the same server processes. `/mcp/static` and `/mcp/dynamic` force the respective mode.

```sh
claude mcp add -s user --transport http emcipi http://127.0.0.1:8808/mcp/static
pi mcp add emcipi --url http://127.0.0.1:8808/mcp/static
```

Clients with their own tool search (Claude Code, Pi) use `static`, others `dynamic`.

## Configuration

A complete, commented example: [`config.example.toml`](config.example.toml).

```toml
mode = "static"
idle_timeout_secs = 600

[servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "${cmd:gh auth token}" }
mode = "dynamic"
exclude_tools = ["delete_*"]

[servers.docs]
url = "https://example.com/mcp"
headers = { Authorization = "Bearer ${file:~/.secrets/docs}" }
```

### Secrets

Secrets stay out of the config. Placeholders fetch them, freshly on every connect:

| Placeholder | Source |
|---|---|
| `${VAR}`, `${VAR:-default}` | environment variable |
| `${file:path}` | file contents (`~/` and relative to the config) |
| `${cmd:program args}` | output of a program, e.g. `op read …`, `gh auth token` |

`${cmd:…}` runs the program directly, without a shell, so it behaves the same on every OS. For pipes, call the shell yourself: `${cmd:sh -c '…'}`.

stdio servers only see a base environment (`PATH`, `HOME`, locale, proxy, …) plus their own `env`. `inherit_env = true` gives them the full environment.

> **macOS:** When an app or launchd starts emcipi, `/opt/homebrew/bin` is usually missing from `PATH`. Use absolute paths or set `PATH`.

## Modes

- **`static`:** every tool is listed directly as `<server>__<tool>`.
- **`dynamic`:** the client only sees `search_tools`, `describe_tools` and `call_tool` and finds tools itself, which saves context with many servers. The search uses BM25 like Pi; see [`docs/research/tool-search.md`](docs/research/tool-search.md).

The mode can be set globally, per CLI flag, per endpoint or per server (`mode = …`). The server setting wins.

## Idle stop and tool cache

With `idle_timeout_secs`, unused stdio servers are stopped together with their child processes, so the `node` behind `npx` goes too. Tool lists are cached on disk, so neither listing tools nor restarting emcipi starts a server; only a real tool call does.

Don't use it for servers that keep session state, such as browser automation or in-memory stores. Set `idle_timeout_secs = 0` for those.

## Releasing

Bump the version in `Cargo.toml`, commit, then run `git tag vX.Y.Z && git push origin vX.Y.Z`. CI builds all platforms and publishes the release.
