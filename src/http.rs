use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{HttpConfig, Mode},
    gateway::{Gateway, Hub},
};

pub struct HttpOptions<'a> {
    pub cfg: &'a HttpConfig,
    /// Already resolved bearer token for inbound requests.
    pub auth_token: Option<String>,
    pub default_mode: Mode,
}

/// Serves `{path}` with the default mode plus `{path}/static` and `{path}/dynamic`,
/// all backed by the same upstream connections.
pub async fn serve(
    hub: Arc<Hub>,
    opts: HttpOptions<'_>,
    shutdown: CancellationToken,
) -> Result<()> {
    let cfg = opts.cfg;
    let mut server_cfg =
        StreamableHttpServerConfig::default().with_cancellation_token(shutdown.child_token());
    if let Some(hosts) = &cfg.allowed_hosts {
        server_cfg = server_cfg.with_allowed_hosts(hosts.clone());
    }
    let service = |mode: Mode| {
        let gateway = Gateway {
            hub: hub.clone(),
            mode,
        };
        StreamableHttpService::new(
            move || Ok(gateway.clone()),
            Arc::new(LocalSessionManager::default()),
            server_cfg.clone(),
        )
    };

    let base = cfg.path.trim_end_matches('/');
    let mut mcp = Router::new()
        .route_service(
            if base.is_empty() { "/" } else { base },
            service(opts.default_mode),
        )
        .route_service(&format!("{base}/static"), service(Mode::Static))
        .route_service(&format!("{base}/dynamic"), service(Mode::Dynamic));
    if let Some(token) = opts.auth_token {
        let expected: Arc<str> = format!("Bearer {token}").into();
        mcp = mcp.layer(middleware::from_fn(move |req: Request, next: Next| {
            let expected = expected.clone();
            async move { require_token(&expected, req, next).await }
        }));
    }
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(mcp);

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("cannot listen on {}", cfg.listen))?;
    tracing::info!(
        "listening on http://{}{base} (default mode: {:?}; also {base}/static, {base}/dynamic)",
        cfg.listen,
        opts.default_mode
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

async fn require_token(expected: &str, req: Request, next: Next) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| constant_time_eq(v.as_bytes(), expected.as_bytes()));
    if authorized {
        next.run(req).await
    } else {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body("unauthorized".into())
            .expect("static response")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
