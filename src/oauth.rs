//! OAuth for HTTP upstreams, following the MCP authorization spec: discovery, dynamic
//! client registration, authorization code with PKCE. rmcp does the protocol work; this
//! module adds the browser login, a file-based token store and the client used to connect.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{Router, http::Uri, response::Html, routing::get};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore,
    OAuthClientConfig, OAuthState, StoredCredentials,
};
use serde::{Deserialize, Serialize};

use crate::{config::ServerConfig, interpolate, platform};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refreshing tokens needs no redirect, but rmcp's client config requires one.
const UNUSED_REDIRECT: &str = "http://127.0.0.1/callback";

/// Where the tokens of `server` are stored.
pub fn credentials_path(server: &str) -> Option<PathBuf> {
    platform::default_data_dir().map(|d| d.join("oauth").join(format!("{server}.json")))
}

/// OAuth applies to HTTP servers unless the config sets an `Authorization` header.
pub fn applies_to(server: &ServerConfig) -> bool {
    server.url.is_some()
        && !server
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
}

/// Tokens of one server, bound to its URL: credentials for another URL are not used.
struct FileCredentialStore {
    path: PathBuf,
    url: String,
    /// Serializes writes of the gateway's refresh tasks.
    write: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct CredentialFile {
    url: String,
    credentials: StoredCredentials,
}

impl FileCredentialStore {
    fn new(server: &str, url: &str) -> Result<Self> {
        let path = credentials_path(server).context("no data directory for credentials")?;
        Ok(Self {
            path,
            url: url.to_owned(),
            write: Mutex::new(()),
        })
    }
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let raw = match std::fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(store_error("read", &self.path, e)),
        };
        let file: CredentialFile =
            serde_json::from_slice(&raw).map_err(|e| store_error("parse", &self.path, e))?;
        Ok((file.url == self.url).then_some(file.credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let _guard = self.write.lock().expect("credential write lock");
        let file = CredentialFile {
            url: self.url.clone(),
            credentials,
        };
        let data =
            serde_json::to_vec_pretty(&file).map_err(|e| store_error("encode", &self.path, e))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| store_error("create", dir, e))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        platform::write_private(&tmp, &data).map_err(|e| store_error("write", &tmp, e))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| store_error("write", &self.path, e))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(store_error("delete", &self.path, e))
            }
            _ => Ok(()),
        }
    }
}

fn store_error(action: &str, path: &Path, e: impl std::fmt::Display) -> AuthError {
    AuthError::InternalError(format!("cannot {action} {}: {e}", path.display()))
}

/// The HTTP client for connecting to `server`. With stored tokens it authenticates and
/// refreshes them; without, requests go out unauthenticated and a 401 surfaces as an
/// error asking for `emcipi login`.
pub async fn client(
    name: &str,
    url: &str,
    server: &ServerConfig,
    base_dir: &Path,
) -> Result<AuthClient<reqwest::Client>> {
    let mut manager = AuthorizationManager::new(url).await?;
    manager.set_credential_store(FileCredentialStore::new(name, url)?);
    if manager.initialize_from_store().await?
        && let Some((client_id, Some(secret))) = client_credentials(server, base_dir).await?
    {
        manager.configure_client(
            OAuthClientConfig::new(client_id, UNUSED_REDIRECT).with_client_secret(secret),
        )?;
    }
    Ok(AuthClient::new(reqwest::Client::new(), manager))
}

/// `(client_id, client_secret)` of a pre-registered client, placeholders resolved.
async fn client_credentials(
    server: &ServerConfig,
    base_dir: &Path,
) -> Result<Option<(String, Option<String>)>> {
    let Some(oauth) = &server.oauth else {
        return Ok(None);
    };
    let Some(client_id) = &oauth.client_id else {
        return Ok(None);
    };
    let secret = match &oauth.client_secret {
        Some(raw) => Some(
            interpolate::resolve(raw, base_dir)
                .await
                .context("oauth.client_secret")?,
        ),
        None => None,
    };
    Ok(Some((client_id.clone(), secret)))
}

/// Runs the browser login for `name` and stores the tokens.
pub async fn login(
    name: &str,
    server: &ServerConfig,
    base_dir: &Path,
    open_browser: bool,
) -> Result<()> {
    if !applies_to(server) {
        bail!("'{name}' is not an HTTP server without an Authorization header");
    }
    let raw_url = server.url.as_deref().expect("checked by applies_to");
    let url = interpolate::resolve(raw_url, base_dir)
        .await
        .context("url")?;
    let oauth = server.oauth.clone().unwrap_or_default();

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", oauth.callback_port.unwrap_or(0)))
        .await
        .context("cannot open the login callback port")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut manager = AuthorizationManager::new(url.as_str()).await?;
    manager.set_credential_store(FileCredentialStore::new(name, &url)?);
    let mut state = OAuthState::Unauthorized(manager);

    let mut request = AuthorizationRequest::new(&redirect_uri).with_client_name("emcipi");
    if !oauth.scopes.is_empty() {
        request = request.with_scopes(oauth.scopes.iter().cloned());
    }
    if let Some((client_id, secret)) = client_credentials(server, base_dir).await? {
        request = request.with_preregistered_client(client_id);
        if let Some(secret) = secret {
            request = request.with_client_secret(secret);
        }
    }
    state
        .start_authorization(request)
        .await
        .context("cannot start the authorization")?;
    let auth_url = state.get_authorization_url().await?;

    eprintln!("Sign in to '{name}' in your browser:\n\n  {auth_url}\n");
    if open_browser {
        platform::open_url(&auth_url);
    }
    let callback = wait_for_callback(listener, LOGIN_TIMEOUT).await?;
    state
        .handle_callback_url(&format!("http://127.0.0.1:{port}{callback}"))
        .await
        .context("authorization failed")?;
    eprintln!("Signed in to '{name}'.");
    Ok(())
}

/// Deletes the stored tokens of `name`. Returns whether there were any.
pub fn logout(name: &str) -> Result<bool> {
    let path = credentials_path(name).context("no data directory for credentials")?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot delete {}", path.display())),
    }
}

/// Serves `/callback` on `listener` until the browser arrives; returns path and query.
async fn wait_for_callback(listener: tokio::net::TcpListener, timeout: Duration) -> Result<String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let app = Router::new().route(
        "/callback",
        get(move |uri: Uri| {
            let tx = tx.clone();
            async move {
                if let Some(tx) = tx.lock().expect("callback lock").take() {
                    let _ = tx.send(uri.to_string());
                }
                Html("<p>emcipi: signed in. You can close this window.</p>")
            }
        }),
    );
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = done_rx.await;
            })
            .await
    });
    let result = tokio::time::timeout(timeout, rx).await;
    let _ = done_tx.send(());
    let _ = server.await;
    match result {
        Ok(Ok(callback)) => Ok(callback),
        Ok(Err(_)) => Err(anyhow!("login callback server stopped")),
        Err(_) => Err(anyhow!("no login within {timeout:?}")),
    }
}

/// Whether an upstream error means the server wants a login. rmcp wraps the 401 in
/// several type-erased transport errors, so the message is matched instead of the type.
pub fn needs_login(error: &anyhow::Error) -> bool {
    let text = format!("{error:#} {error:?}");
    ["Auth required", "AuthRequired", "AuthorizationRequired"]
        .iter()
        .any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(toml: &str) -> ServerConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn oauth_only_without_authorization_header() {
        assert!(applies_to(&server("url = \"https://x/mcp\"")));
        assert!(!applies_to(&server(
            "url = \"https://x/mcp\"\nheaders = { authorization = \"Bearer t\" }"
        )));
        assert!(!applies_to(&server("command = \"x\"")));
    }

    #[test]
    fn credentials_are_bound_to_the_url() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("emcipi-oauth-test-{}", std::process::id()));
        let store = |url: &str| FileCredentialStore {
            path: dir.join("x.json"),
            url: url.to_owned(),
            write: Mutex::new(()),
        };
        let creds = StoredCredentials::new("client-1".to_owned(), None, vec![], None);
        rt.block_on(store("https://a/mcp").save(creds)).unwrap();
        let loaded = rt.block_on(store("https://a/mcp").load()).unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-1");
        assert!(
            rt.block_on(store("https://b/mcp").load())
                .unwrap()
                .is_none()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("x.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        rt.block_on(store("https://a/mcp").clear()).unwrap();
        assert!(
            rt.block_on(store("https://a/mcp").load())
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
