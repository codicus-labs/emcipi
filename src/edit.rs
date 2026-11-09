//! `emcipi add` and `emcipi remove`: edit the `[servers]` of the config file while keeping
//! its comments and formatting. The result is validated before anything is written.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use crate::config::{Config, Mode};

/// A server as given on the command line.
#[derive(Debug, Default)]
pub struct NewServer {
    pub url: Option<String>,
    /// Program and arguments of a stdio server.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub mode: Option<Mode>,
    pub description: Option<String>,
    pub disabled: bool,
}

/// Adds `server` as `name` to the config at `path`, creating the file if needed.
/// Returns whether an existing server was replaced.
pub fn add(path: &Path, name: &str, server: &NewServer, force: bool) -> Result<bool> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let (updated, replaced) = add_to(&raw, name, server, force)?;
    Config::parse(&updated, path).context("refusing to write an invalid config")?;
    write(path, &updated)?;
    Ok(replaced)
}

/// Removes the server `name` from the config at `path`.
pub fn remove(path: &Path, name: &str) -> Result<()> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let updated = remove_from(&raw, name)?;
    Config::parse(&updated, path).context("refusing to write an invalid config")?;
    write(path, &updated)
}

fn add_to(raw: &str, name: &str, server: &NewServer, force: bool) -> Result<(String, bool)> {
    if server.url.is_some() && !server.env.is_empty() {
        bail!("--env is for stdio servers; HTTP servers take --header");
    }
    if !server.command.is_empty() && !server.headers.is_empty() {
        bail!("--header is for HTTP servers; stdio servers take --env");
    }
    let mut doc: DocumentMut = raw.parse().context("cannot parse the config")?;
    let servers = servers_table(&mut doc, true)?;
    let mut table = to_table(server);
    let replaced = match servers.get_mut(name) {
        Some(_) if !force => bail!("server '{name}' already exists; use --force to replace it"),
        Some(Item::Table(old)) => {
            // Keep its place in the file and the comments above it.
            table.set_position(old.position());
            *table.decor_mut() = old.decor().clone();
            *old = table;
            true
        }
        Some(_) => bail!("servers.{name} is not a table; edit the config by hand"),
        None => {
            if !doc_is_empty(raw) {
                table.decor_mut().set_prefix("\n");
            }
            servers.insert(name, Item::Table(table));
            false
        }
    };
    Ok((doc.to_string(), replaced))
}

fn remove_from(raw: &str, name: &str) -> Result<String> {
    let mut doc: DocumentMut = raw.parse().context("cannot parse the config")?;
    let servers = servers_table(&mut doc, false)?;
    if servers.remove(name).is_none() {
        bail!("no server '{name}' in the config");
    }
    Ok(doc.to_string())
}

/// The `[servers]` table; created (implicitly, without a header of its own) if `create`.
fn servers_table(doc: &mut DocumentMut, create: bool) -> Result<&mut Table> {
    if create && !doc.contains_key("servers") {
        let mut table = Table::new();
        table.set_implicit(true);
        doc.insert("servers", Item::Table(table));
    }
    match doc.get_mut("servers") {
        Some(Item::Table(table)) => Ok(table),
        Some(_) => bail!("`servers` is not a table; edit the config by hand"),
        None => bail!("the config has no servers"),
    }
}

fn to_table(server: &NewServer) -> Table {
    let mut table = Table::new();
    if server.disabled {
        table["enabled"] = value(false);
    }
    if let Some(description) = &server.description {
        table["description"] = value(description);
    }
    if let Some(mode) = server.mode {
        table["mode"] = value(mode.as_str());
    }
    if let Some((program, args)) = server.command.split_first() {
        table["command"] = value(program);
        if !args.is_empty() {
            table["args"] = value(Array::from_iter(args));
        }
        if !server.env.is_empty() {
            table["env"] = value(inline_table(&server.env));
        }
    }
    if let Some(url) = &server.url {
        table["url"] = value(url);
        if !server.headers.is_empty() {
            table["headers"] = value(inline_table(&server.headers));
        }
    }
    table
}

fn inline_table(pairs: &[(String, String)]) -> InlineTable {
    InlineTable::from_iter(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

fn doc_is_empty(raw: &str) -> bool {
    raw.trim().is_empty()
}

/// Writes through symlinks (dotfile managers) via a temporary file and a rename, keeping
/// the permissions of the existing file.
fn write(path: &Path, contents: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = target.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, contents).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).with_context(|| format!("cannot write {}", target.display()))
}

/// Parses `KEY=VALUE` (for `--env`).
pub fn parse_env(arg: &str) -> Result<(String, String), String> {
    match arg.split_once('=') {
        Some((key, val)) if !key.is_empty() => Ok((key.to_owned(), val.to_owned())),
        _ => Err("expected KEY=VALUE".to_owned()),
    }
}

/// Parses `Name: value` (for `--header`), like curl.
pub fn parse_header(arg: &str) -> Result<(String, String), String> {
    match arg.split_once(':') {
        Some((name, val)) if !name.trim().is_empty() => {
            Ok((name.trim().to_owned(), val.trim().to_owned()))
        }
        _ => Err("expected 'Name: value'".to_owned()),
    }
}

/// Names of values that look like literal secrets, to warn about them.
pub fn literal_secrets(server: &NewServer) -> Vec<String> {
    const HINTS: [&str; 6] = [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "AUTHORIZATION",
        "AUTH",
    ];
    let env = server.env.iter().map(|(k, v)| (format!("env {k}"), k, v));
    let headers = server
        .headers
        .iter()
        .map(|(k, v)| (format!("header {k}"), k, v));
    env.chain(headers)
        .filter(|(_, key, val)| {
            let key = key.to_ascii_uppercase();
            HINTS.iter().any(|h| key.contains(h)) && !val.contains("${")
        })
        .map(|(label, _, _)| label)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"# my gateway
mode = "static"

# docs, keep this comment
[servers.docs]
url = "https://example.com/mcp"  # trailing comment

[servers.fs]
command = "npx"
"#;

    fn stdio(command: &[&str]) -> NewServer {
        NewServer {
            command: command.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn check(raw: &str) -> Config {
        Config::parse(raw, Path::new("config.toml")).unwrap()
    }

    #[test]
    fn add_keeps_comments_and_appends() {
        let server = NewServer {
            env: vec![("TOKEN".into(), "${cmd:gh auth token}".into())],
            mode: Some(Mode::Dynamic),
            ..stdio(&["uvx", "mcp-server-fetch", "--flag"])
        };
        let (out, replaced) = add_to(CONFIG, "fetch", &server, false).unwrap();
        assert!(!replaced);
        assert!(
            out.starts_with(CONFIG),
            "existing content unchanged:\n{out}"
        );
        assert!(
            out.ends_with(
                "\n[servers.fetch]\nmode = \"dynamic\"\ncommand = \"uvx\"\n\
                 args = [\"mcp-server-fetch\", \"--flag\"]\n\
                 env = { TOKEN = \"${cmd:gh auth token}\" }\n"
            ),
            "{out}"
        );
        let cfg = check(&out);
        assert_eq!(cfg.servers["fetch"].args, ["mcp-server-fetch", "--flag"]);
        assert_eq!(cfg.servers["fetch"].mode, Some(Mode::Dynamic));
    }

    #[test]
    fn add_to_an_empty_file() {
        let server = NewServer {
            url: Some("https://x/mcp".into()),
            headers: vec![("Authorization".into(), "Bearer ${file:~/t}".into())],
            description: Some("X".into()),
            disabled: true,
            ..Default::default()
        };
        let (out, _) = add_to("", "x", &server, false).unwrap();
        assert_eq!(
            out,
            "[servers.x]\nenabled = false\ndescription = \"X\"\nurl = \"https://x/mcp\"\n\
             headers = { Authorization = \"Bearer ${file:~/t}\" }\n"
        );
        assert!(!check(&out).servers["x"].enabled);
    }

    #[test]
    fn add_refuses_to_overwrite_without_force() {
        let err = add_to(CONFIG, "docs", &stdio(&["x"]), false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");

        let (out, replaced) = add_to(CONFIG, "docs", &stdio(&["my-docs"]), true).unwrap();
        assert!(replaced);
        assert!(out.contains("# docs, keep this comment\n[servers.docs]\ncommand = \"my-docs\"\n"));
        assert!(!out.contains("example.com"));
        let order: Vec<_> = check(&out).servers.into_keys().collect();
        assert_eq!(order, ["docs", "fs"]);
        assert!(out.find("[servers.docs]") < out.find("[servers.fs]"));
    }

    #[test]
    fn options_must_match_the_transport() {
        let server = NewServer {
            headers: vec![("A".into(), "b".into())],
            ..stdio(&["x"])
        };
        assert!(add_to("", "x", &server, false).is_err());
        let server = NewServer {
            url: Some("https://x".into()),
            env: vec![("A".into(), "b".into())],
            ..Default::default()
        };
        assert!(add_to("", "x", &server, false).is_err());
    }

    #[test]
    fn remove_keeps_the_rest() {
        let out = remove_from(CONFIG, "docs").unwrap();
        assert!(!out.contains("servers.docs") && !out.contains("example.com"));
        assert!(out.starts_with("# my gateway\nmode = \"static\"\n"));
        assert!(out.contains("[servers.fs]\ncommand = \"npx\"\n"));
        check(&out);

        let err = remove_from(CONFIG, "nope").unwrap_err();
        assert!(err.to_string().contains("no server 'nope'"));
    }

    #[test]
    fn add_writes_the_file_and_rejects_invalid_names() {
        let dir = std::env::temp_dir().join(format!("emcipi-edit-test-{}", std::process::id()));
        let path = dir.join("sub").join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        add(&path, "fs", &stdio(&["npx"]), false).unwrap();
        assert!(check(&std::fs::read_to_string(&path).unwrap()).servers["fs"].is_stdio());

        let err = add(&path, "bad name", &stdio(&["npx"]), false).unwrap_err();
        assert!(format!("{err:#}").contains("may only contain"), "{err:#}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("bad name"));

        remove(&path, "fs").unwrap();
        assert!(
            check(&std::fs::read_to_string(&path).unwrap())
                .servers
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn argument_parsers() {
        assert_eq!(parse_env("A=b=c").unwrap(), ("A".into(), "b=c".into()));
        assert!(parse_env("=x").is_err() && parse_env("x").is_err());
        assert_eq!(
            parse_header("Authorization: Bearer ${cmd:x}").unwrap(),
            ("Authorization".into(), "Bearer ${cmd:x}".into())
        );
        assert!(parse_header("nocolon").is_err());
    }

    #[test]
    fn warns_about_literal_secrets_only() {
        let server = NewServer {
            env: vec![
                ("API_KEY".into(), "abc".into()),
                ("GITHUB_TOKEN".into(), "${cmd:gh auth token}".into()),
                ("DEBUG".into(), "1".into()),
            ],
            headers: vec![("Authorization".into(), "Bearer abc".into())],
            ..Default::default()
        };
        assert_eq!(
            literal_secrets(&server),
            ["env API_KEY", "header Authorization"]
        );
    }
}
