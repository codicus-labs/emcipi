//! `emcipi list`: a one-line-per-server overview, or the tool names of one server.

use std::{collections::BTreeMap, io::IsTerminal};

use anyhow::{Context, Result, bail};
use futures::future::join_all;

use crate::{
    config::{Config, Mode, Overrides},
    gateway::Hub,
    oauth,
    upstream::LOGIN_REQUIRED,
};

struct Row {
    name: String,
    transport: &'static str,
    mode: Option<Mode>,
    tools: Option<usize>,
    status: String,
}

/// Prints all configured servers. Without `refresh` nothing is started: tool counts come
/// from the cache. With `refresh` every enabled server is connected and its cache updated.
pub async fn servers(cfg: Config, overrides: Overrides, refresh: bool) -> Result<()> {
    let configured = cfg.servers.clone();
    if configured.is_empty() {
        println!("No servers configured. Add one with `emcipi add`.");
        return Ok(());
    }
    let hub = Hub::new(cfg, overrides);
    let state = hub.state();
    let mut refreshed: BTreeMap<String, Result<usize>> = if refresh {
        join_all(
            state
                .upstreams()
                .iter()
                .map(|u| async move { (u.name.clone(), u.refresh_tools().await.map(|t| t.len())) }),
        )
        .await
        .into_iter()
        .collect()
    } else {
        BTreeMap::new()
    };

    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for (name, server) in configured {
        let transport = if server.is_stdio() { "stdio" } else { "http" };
        let upstream = state.upstreams().iter().find(|u| u.name == name);
        let Some(upstream) = upstream else {
            rows.push(Row {
                name,
                transport,
                mode: None,
                tools: None,
                status: "disabled".to_owned(),
            });
            continue;
        };
        let (tools, status) = match refreshed.remove(&name) {
            Some(Ok(count)) => (Some(count), "ok".to_owned()),
            Some(Err(e)) => {
                let message = format!("{e:#}");
                let status = if message.contains(LOGIN_REQUIRED) {
                    format!("login required: emcipi login {name}")
                } else {
                    errors.push(message);
                    "error".to_owned()
                };
                (None, status)
            }
            None => match upstream.cached_tools().await {
                Some(tools) => {
                    let signed_in = oauth::applies_to(&server)
                        && oauth::credentials_path(&name).is_some_and(|p| p.exists());
                    let status = if signed_in {
                        "cached, signed in"
                    } else {
                        "cached"
                    };
                    (Some(tools.len()), status.to_owned())
                }
                None => (None, "not cached".to_owned()),
            },
        };
        rows.push(Row {
            name,
            transport,
            mode: Some(upstream.cfg.mode(state.default_mode)),
            tools,
            status,
        });
    }
    hub.shutdown().await;

    print!("{}", render(&rows));
    if !refresh && rows.iter().any(|r| r.status == "not cached") {
        println!("\nRun `emcipi list --refresh` to connect all servers and cache their tools.");
    }
    if !errors.is_empty() {
        println!();
        for error in &errors {
            println!("{error}");
        }
        bail!("{} of {} servers failed", errors.len(), rows.len());
    }
    Ok(())
}

/// Prints the tool names of `name`: from the cache, else (or with `refresh`) from the server.
pub async fn tools(cfg: Config, overrides: Overrides, name: &str, refresh: bool) -> Result<()> {
    let server = cfg
        .servers
        .get(name)
        .with_context(|| format!("no server '{name}' in the config"))?;
    if !server.enabled {
        bail!("server '{name}' is disabled");
    }
    let hub = Hub::new(cfg, overrides);
    let upstream = hub
        .state()
        .upstreams()
        .iter()
        .find(|u| u.name == name)
        .cloned()
        .expect("enabled servers have an upstream");
    let result = if refresh {
        upstream.refresh_tools().await
    } else {
        upstream.tools().await
    };
    hub.shutdown().await;
    let mut names: Vec<String> = result?.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    let width = std::io::stdout()
        .is_terminal()
        .then(|| terminal_size::terminal_size().map(|(w, _)| usize::from(w.0)))
        .flatten();
    print!("{}", columns(&names, width));
    Ok(())
}

fn render(rows: &[Row]) -> String {
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                r.name.clone(),
                r.transport.to_owned(),
                r.mode.map_or("-", Mode::as_str).to_owned(),
                r.tools.map_or("-".to_owned(), |n| n.to_string()),
                r.status.clone(),
            ]
        })
        .collect();
    let header = ["NAME", "TRANSPORT", "MODE", "TOOLS", "STATUS"].map(str::to_owned);
    let mut widths = [0; 5];
    for row in std::iter::once(&header).chain(&cells) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in std::iter::once(&header).chain(&cells) {
        let line = format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:>w3$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        );
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `names` in columns filling `width`, top to bottom like `ls`; one per line without a width.
fn columns(names: &[String], width: Option<usize>) -> String {
    let cell = names.iter().map(|n| n.chars().count()).max().unwrap_or(0) + 2;
    let cols = width.map_or(1, |w| (w / cell).max(1));
    let rows = names.len().div_ceil(cols);
    let mut out = String::new();
    for row in 0..rows {
        let line: String = (0..cols)
            .filter_map(|col| names.get(col * rows + row))
            .map(|name| format!("{name:<cell$}"))
            .collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_aligned() {
        let rows = [
            Row {
                name: "github".into(),
                transport: "http",
                mode: Some(Mode::Dynamic),
                tools: Some(104),
                status: "cached".into(),
            },
            Row {
                name: "fs".into(),
                transport: "stdio",
                mode: None,
                tools: None,
                status: "disabled".into(),
            },
        ];
        assert_eq!(
            render(&rows),
            "NAME    TRANSPORT  MODE     TOOLS  STATUS\n\
             github  http       dynamic    104  cached\n\
             fs      stdio      -            -  disabled\n"
        );
    }

    #[test]
    fn names_fill_columns_top_to_bottom() {
        let names: Vec<String> = ["a", "bb", "c", "d", "e"].map(String::from).into();
        assert_eq!(columns(&names, Some(9)), "a   d\nbb  e\nc\n");
        assert_eq!(columns(&names, None), "a\nbb\nc\nd\ne\n");
        assert_eq!(columns(&[], Some(80)), "");
    }
}
