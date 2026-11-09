//! `${...}` placeholders in config values, resolved when a value is needed:
//!
//! - `${VAR}`, `${env:VAR}`, `${VAR:-default}`: environment variable
//! - `${file:path}`: file contents (`~/` and paths relative to the config file allowed)
//! - `${cmd:program args...}`: stdout of the program, started directly without a shell,
//!   so it behaves the same on Linux, macOS and Windows (see [`split_words`])
//! - `$${`: a literal `${`
//!
//! File and command output is stripped of trailing whitespace.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, anyhow, bail};

use crate::platform;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq)]
enum Segment<'a> {
    Literal(&'a str),
    Env {
        var: &'a str,
        default: Option<&'a str>,
    },
    File(&'a str),
    Cmd(&'a str),
}

/// Checks the syntax without reading anything, so typos fail at startup.
pub fn check(input: &str) -> Result<()> {
    parse(input).map(|_| ())
}

pub async fn resolve(input: &str, base_dir: &Path) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    for segment in parse(input)? {
        match segment {
            Segment::Literal(text) => out.push_str(text),
            Segment::Env { var, default } => out.push_str(&env(var, default)?),
            Segment::File(path) => out.push_str(&read_file(path, base_dir).await?),
            Segment::Cmd(command) => out.push_str(&run_command(command, base_dir).await?),
        }
    }
    Ok(out)
}

fn parse(input: &str) -> Result<Vec<Segment<'_>>> {
    let mut segments = Vec::new();
    let mut rest = input;
    while let Some(pos) = rest.find('$') {
        let (before, from_dollar) = rest.split_at(pos);
        if let Some(after) = from_dollar.strip_prefix("$${") {
            segments.push(Segment::Literal(before));
            segments.push(Segment::Literal("${"));
            rest = after;
        } else if let Some(after) = from_dollar.strip_prefix("${") {
            segments.push(Segment::Literal(before));
            let end = closing_brace(after).ok_or_else(|| {
                if after.starts_with("cmd:") {
                    anyhow!("unterminated '${{' or quote in '{input}'")
                } else {
                    anyhow!("unterminated '${{' in '{input}'")
                }
            })?;
            segments.push(placeholder(&after[..end])?);
            rest = &after[end + 1..];
        } else {
            segments.push(Segment::Literal(before));
            segments.push(Segment::Literal("$"));
            rest = &from_dollar[1..];
        }
    }
    segments.push(Segment::Literal(rest));
    segments.retain(|s| *s != Segment::Literal(""));
    Ok(segments)
}

/// Index of the `}` closing a placeholder. Nested braces are balanced, and in `cmd:`
/// placeholders braces inside quotes are ignored (e.g. `sh -c 'awk "{print $1}"'`).
fn closing_brace(text: &str) -> Option<usize> {
    let quote_aware = text.starts_with("cmd:");
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') if quote_aware => quote = Some(c),
            (None, '{') => depth += 1,
            (None, '}') if depth == 0 => return Some(i),
            (None, '}') => depth -= 1,
            _ => {}
        }
    }
    None
}

fn placeholder(expr: &str) -> Result<Segment<'_>> {
    if let Some(path) = expr.strip_prefix("file:") {
        if path.trim().is_empty() {
            bail!("empty path in '${{file:}}'");
        }
        return Ok(Segment::File(path.trim()));
    }
    if let Some(command) = expr.strip_prefix("cmd:") {
        let words = split_words(command).with_context(|| format!("in '${{{expr}}}'"))?;
        if words.is_empty() {
            bail!("empty command in '${{cmd:}}'");
        }
        return Ok(Segment::Cmd(command));
    }
    let expr = expr.strip_prefix("env:").unwrap_or(expr);
    let (var, default) = match expr.split_once(":-") {
        Some((var, default)) => (var, Some(default)),
        None => (expr, None),
    };
    let valid = !var.is_empty() && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!(
            "invalid placeholder '${{{expr}}}' (expected VAR, env:VAR, file:PATH or cmd:COMMAND)"
        );
    }
    Ok(Segment::Env { var, default })
}

/// Splits a command line into words, the same way on every platform:
///
/// - whitespace separates words
/// - `'...'` is taken literally
/// - `"..."` is taken literally except for `\"` and `\\`
/// - a backslash outside quotes is an ordinary character, so Windows paths work unquoted
///
/// There are no pipes, redirections or variables: the result is the program and its
/// arguments. For shell features call the shell explicitly, e.g. `sh -c '...'`.
pub fn split_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => bail!("unterminated single quote"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\\')) => {
                            current.push(chars.next().expect("peeked"));
                        }
                        Some(c) => current.push(c),
                        None => bail!("unterminated double quote"),
                    }
                }
            }
            c => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

fn env(var: &str, default: Option<&str>) -> Result<String> {
    match (std::env::var(var), default) {
        (Ok(value), None) => Ok(value),
        (Ok(value), Some(_)) if !value.is_empty() => Ok(value),
        (_, Some(default)) => Ok(default.to_owned()),
        (Err(_), None) => bail!("environment variable '{var}' is not set"),
    }
}

async fn read_file(path: &str, base_dir: &Path) -> Result<String> {
    let full = platform::expand_path(path, base_dir);
    let content = tokio::fs::read_to_string(&full)
        .await
        .with_context(|| format!("cannot read secret file {}", full.display()))?;
    Ok(content.trim_end().to_owned())
}

async fn run_command(command: &str, base_dir: &Path) -> Result<String> {
    let words = split_words(command)?;
    let (program, args) = words.split_first().context("empty command")?;
    let resolved = platform::resolve_program(program, None, base_dir)?;
    let child = tokio::process::Command::new(&resolved)
        .args(args)
        .current_dir(base_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot run '{program}'"))?;
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("command timed out after {COMMAND_TIMEOUT:?}: {command}"))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "command failed ({}): {command}: {}",
            output.status,
            stderr.trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("command output is not UTF-8")?;
    Ok(stdout.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_now(input: &str) -> Result<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(resolve(input, &std::env::temp_dir()))
    }

    #[test]
    fn env_values() {
        // SAFETY: no other test touches this variable.
        unsafe { std::env::set_var("EMCIPI_TEST_VAR", "secret") };
        assert_eq!(
            resolve_now("Bearer ${EMCIPI_TEST_VAR}").unwrap(),
            "Bearer secret"
        );
        assert_eq!(resolve_now("${env:EMCIPI_TEST_VAR}").unwrap(), "secret");
        assert_eq!(
            resolve_now("${EMCIPI_UNSET_VAR:-fallback}").unwrap(),
            "fallback"
        );
        assert_eq!(resolve_now("${env:EMCIPI_UNSET_VAR:-}").unwrap(), "");
        assert!(resolve_now("${EMCIPI_UNSET_VAR}").is_err());
    }

    #[test]
    fn literals() {
        assert_eq!(resolve_now("$${literal}").unwrap(), "${literal}");
        assert_eq!(resolve_now("price: $5 and $").unwrap(), "price: $5 and $");
        assert_eq!(resolve_now("plain").unwrap(), "plain");
    }

    #[test]
    fn word_splitting() {
        assert_eq!(
            split_words(r#"op read "op://Private/My Item/token""#).unwrap(),
            ["op", "read", "op://Private/My Item/token"]
        );
        assert_eq!(
            split_words(r#"  a  'b c'  "d \"e\" \\ f" g'h'"#).unwrap(),
            ["a", "b c", r#"d "e" \ f"#, "gh"]
        );
        assert_eq!(
            split_words(r"C:\Tools\op.exe read x").unwrap(),
            [r"C:\Tools\op.exe", "read", "x"]
        );
        assert_eq!(split_words(r#"a "" b"#).unwrap(), ["a", "", "b"]);
        assert!(split_words("a 'open").is_err());
        assert!(split_words(r#"a "open"#).is_err());
        assert!(split_words("  ").unwrap().is_empty());
    }

    #[test]
    fn commands_run_without_shell() {
        let version = resolve_now("${cmd:rustc --version}").unwrap();
        assert!(version.starts_with("rustc "), "{version}");
        assert!(!version.ends_with('\n'));
        let err = format!(
            "{:#}",
            resolve_now("${cmd:rustc --emcipi-bogus-flag}").unwrap_err()
        );
        assert!(err.contains("emcipi-bogus-flag"), "{err}");
        let err = format!(
            "{:#}",
            resolve_now("${cmd:emcipi-no-such-program}").unwrap_err()
        );
        assert!(err.contains("not found"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn explicit_shell_with_braces_in_quotes() {
        assert_eq!(
            resolve_now(r#"${cmd:sh -c "echo 'a b' | awk '{print $2}'"}"#).unwrap(),
            "b"
        );
        assert_eq!(
            resolve_now("x-${cmd:printf 'tok\\n\\n'}-y").unwrap(),
            "x-tok-y"
        );
    }

    #[cfg(windows)]
    #[test]
    fn explicit_shell_on_windows() {
        assert_eq!(resolve_now("${cmd:cmd /C echo hi}").unwrap(), "hi");
    }

    #[test]
    fn files() {
        let dir = std::env::temp_dir().join(format!("emcipi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "file-secret\n").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            rt.block_on(resolve("${file:token}", &dir)).unwrap(),
            "file-secret"
        );
        assert!(rt.block_on(resolve("${file:missing}", &dir)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn syntax_errors() {
        assert!(check("${OPEN").is_err());
        assert!(check("${bad var}").is_err());
        assert!(check("${file:}").is_err());
        assert!(check("${cmd: }").is_err());
        assert!(check("${cmd:op read 'unterminated}").is_err());
        assert!(check("ok ${A} ${file:x} ${cmd:y}").is_ok());
    }
}
