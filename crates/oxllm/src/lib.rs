//! oxllm library — shared router, state, and middleware.
//! Used by both the CLI binary (`main.rs`) and the Shuttle binary
//! (`crates/oxllm-shuttle`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use tower_http::cors::{Any, CorsLayer};

use oxllm_core::config::Config;
use oxllm_core::error::OxllmError;
use oxllm_core::state::{AppState, CircuitState, ProviderState};
use oxllm_core::telemetry::{TelemetryClient, TelemetryWorker};
use reqwest::Url;

pub mod routes;

/// Where a reload should load configuration from.
#[derive(Clone)]
pub enum ConfigSource {
    File(PathBuf),
    Env,
}

impl ConfigSource {
    fn load(&self) -> Result<Config, String> {
        match self {
            ConfigSource::File(path) => {
                Config::load_from_file(path).map_err(|e| e.to_string())
            },
            ConfigSource::Env => Config::from_env().map_err(|e| e.to_string()),
        }
    }
}

#[derive(Clone)]
pub struct Reloader {
    pub sender: tokio::sync::watch::Sender<Arc<AppState>>,
    pub source: ConfigSource,
}

#[derive(Clone)]
pub struct ReloadableState {
    pub app_state: tokio::sync::watch::Receiver<Arc<AppState>>,
    pub telemetry: TelemetryClient,
    pub start_time: Instant,
    pub reloader: Reloader,
}

impl axum::extract::FromRef<ReloadableState> for Arc<AppState> {
    fn from_ref(state: &ReloadableState) -> Self {
        state.app_state.borrow().clone()
    }
}

impl axum::extract::FromRef<ReloadableState> for (Arc<AppState>, TelemetryClient) {
    fn from_ref(state: &ReloadableState) -> Self {
        (state.app_state.borrow().clone(), state.telemetry.clone())
    }
}

impl axum::extract::FromRef<ReloadableState> for Instant {
    fn from_ref(state: &ReloadableState) -> Self {
        state.start_time
    }
}

impl axum::extract::FromRef<ReloadableState> for (Arc<AppState>, Instant) {
    fn from_ref(state: &ReloadableState) -> Self {
        (state.app_state.borrow().clone(), state.start_time)
    }
}

impl axum::extract::FromRef<ReloadableState> for Reloader {
    fn from_ref(state: &ReloadableState) -> Self {
        state.reloader.clone()
    }
}

pub fn build_app_state(config: Config) -> Result<AppState, OxllmError> {
    let mut providers = Vec::new();
    for p in config.providers {
        if !p.enabled {
            continue;
        }
        let url = Url::parse(&p.base_url).map_err(|e| {
            OxllmError::ConfigLoad(format!(
                "Invalid base URL '{}' for provider '{}': {}",
                p.base_url, p.name, e
            ))
        })?;

        providers.push(ProviderState {
            name: p.name,
            base_url: url,
            api_key: p.api_key,
            models: p.models,
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        });
    }

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| {
            OxllmError::ConfigLoad(format!("Failed to build HTTP client: {}", e))
        })?;

    Ok(AppState {
        providers,
        virtual_models: config.virtual_models,
        http_client,
        upstream_timeout_secs: config.server.upstream_timeout_secs,
        api_key: config.server.api_key,
    })
}

/// Builds the full reloadable state: config → validate → AppState → watch channel
/// → telemetry worker → Reloader.
pub fn build_reloadable_state(
    config: Config,
    source: ConfigSource,
) -> Result<ReloadableState, OxllmError> {
    config.validate()?;
    let app_state = Arc::new(build_app_state(config.clone())?);

    let (watch_sender, watch_receiver) = tokio::sync::watch::channel(app_state.clone());

    let (telemetry_tx, telemetry_rx) = tokio::sync::mpsc::channel(1024);
    let telemetry_client = TelemetryClient::new(telemetry_tx);
    let otel_endpoint = config.server.otel_endpoint.clone();
    let _worker_handle = TelemetryWorker::spawn(&otel_endpoint, telemetry_rx)?;

    let reloader = Reloader {
        sender: watch_sender,
        source,
    };

    Ok(ReloadableState {
        app_state: watch_receiver,
        telemetry: telemetry_client,
        start_time: Instant::now(),
        reloader,
    })
}

/// Generates a random request ID for response correlation.
fn generate_request_id() -> String {
    format!("oxllm-{:016x}", rand::random::<u64>())
}

/// Middleware that adds an `x-request-id` header to every response.
/// Generates the ID before calling the handler, stores it in request
/// extensions so route handlers can read the same ID for logs/telemetry,
/// and inserts it into the response header (without overriding an existing
/// `x-request-id` forwarded from upstream).
pub async fn add_request_id(mut req: Request<Body>, next: Next) -> Response {
    let request_id = generate_request_id();
    req.extensions_mut().insert(request_id.clone());
    let mut response = next.run(req).await;
    if !response.headers().contains_key("x-request-id") {
        // SAFETY: generate_request_id produces only ASCII hex chars and "oxllm-" prefix.
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&request_id)
                .expect("generated request ID contains invalid characters"),
        );
    }
    response
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

pub async fn handle_http_reload(
    axum::extract::State(reloader): axum::extract::State<Reloader>,
) -> impl IntoResponse {
    let config = match reloader.source.load() {
        Ok(c) => c,
        Err(e) => {
            let body = serde_json::json!({
                "error": {
                    "message": format!("Failed to load config: {}", e),
                    "type": "invalid_request_error",
                    "code": 400
                }
            });
            let bytes = serde_json::to_vec(&body).unwrap();
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            return response;
        },
    };
    if let Err(e) = config.validate() {
        let body = serde_json::json!({
            "error": {
                "message": format!("Config validation failed: {}", e),
                "type": "invalid_request_error",
                "code": 400
            }
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return response;
    }
    match build_app_state(config) {
        Ok(new_state) => {
            if reloader.sender.send(Arc::new(new_state)).is_ok() {
                info!("Configuration reloaded via HTTP POST /reload");
                let body = serde_json::json!({
                    "message": "Configuration reloaded successfully"
                });
                let bytes = serde_json::to_vec(&body).unwrap();
                let mut response = Response::new(Body::from(bytes));
                *response.status_mut() = StatusCode::OK;
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                response
            } else {
                let body = serde_json::json!({
                    "error": {
                        "message": "Failed to send config update",
                        "type": "internal_error",
                        "code": 500
                    }
                });
                let bytes = serde_json::to_vec(&body).unwrap();
                let mut response = Response::new(Body::from(bytes));
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                response
            }
        },
        Err(e) => {
            let body = serde_json::json!({
                "error": {
                    "message": format!("Failed to build state: {}", e),
                    "type": "invalid_request_error",
                    "code": 400
                }
            });
            let bytes = serde_json::to_vec(&body).unwrap();
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        },
    }
}

pub async fn handle_sighup(reloader: Reloader) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to register SIGHUP handler: {}", e);
                return;
            },
        };

        info!("Registered SIGHUP reload listener");
        while sig.recv().await.is_some() {
            info!("SIGHUP received, reloading configuration...");
            match reloader.source.load() {
                Ok(new_config) => {
                    if let Err(e) = new_config.validate() {
                        error!("Configuration validation failed during hot-reload: {}", e);
                        continue;
                    }
                    match build_app_state(new_config) {
                        Ok(new_state) => {
                            if let Err(e) = reloader.sender.send(Arc::new(new_state)) {
                                error!("Failed to update watch channel: {}", e);
                            } else {
                                info!("Configuration successfully reloaded!");
                            }
                        },
                        Err(e) => {
                            error!("Failed to build new app state during hot-reload: {}", e);
                        },
                    }
                },
                Err(e) => {
                    error!("Failed to load config during hot-reload: {}", e);
                },
            }
        }
    }
}

/// Constant-time check of a `Bearer <token>` Authorization header against the expected key.
/// An empty expected key means auth is disabled → always allowed.
fn check_bearer(auth_header: Option<&str>, expected: &str) -> bool {
    if expected.is_empty() {
        return true;
    }
    match auth_header {
        Some(h) => {
            let token = h.strip_prefix("Bearer ").unwrap_or("");
            token.as_bytes().ct_eq(expected.as_bytes()).into()
        },
        None => false,
    }
}

/// Returns true when the path belongs to the local-only administrative surface.
fn is_admin_path(path: &str) -> bool {
    path.starts_with("/admin/") || path == "/status" || path == "/health" || path == "/reload"
}

/// Middleware: protects routes with the oxllm API key.
///
/// - If `server.api_key` is empty → administrative routes stay localhost-only
///   (preserving the current CLI behavior); `/v1/*` stays open.
/// - Requests from loopback → pass through (local CLI `status`/`provider` works).
/// - Otherwise require `Authorization: Bearer <api_key>` (constant-time compare).
/// - `OPTIONS` preflight requests pass without auth (browser CORS).
pub async fn auth_or_localhost(
    State(app_state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let expected = app_state.api_key.as_str();

    if req.method() == axum::http::Method::OPTIONS {
        return Ok(next.run(req).await);
    }

    let is_local = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_canonical().is_loopback(),
    };

    if is_local {
        return Ok(next.run(req).await);
    }

    // No API key configured: admin routes stay localhost-only (current CLI
    // behavior preserved); /v1/* stays open.
    if expected.is_empty() {
        if is_admin_path(req.uri().path()) {
            warn!(target: "oxllm::security", "Blocked external attempt to access administrative route from IP: {}", addr.ip());
            let body = serde_json::json!({
                "error": {
                    "message": "Access denied: administrative routes are localhost-only",
                    "type": "forbidden",
                    "code": 403
                }
            });
            let bytes = serde_json::to_vec(&body).unwrap();
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::FORBIDDEN;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            return Err(response);
        }
        return Ok(next.run(req).await);
    }

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if check_bearer(auth_header, expected) {
        return Ok(next.run(req).await);
    }

    let body = serde_json::json!({
        "error": {
            "message": "Unauthorized: provide a valid Bearer token",
            "type": "invalid_api_key",
            "code": 401
        }
    });
    let bytes = serde_json::to_vec(&body).unwrap();
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Err(response)
}

/// Builds the full Axum router with all routes, auth, CORS, and request-id middleware.
pub fn build_router(state: ReloadableState) -> Router {
    use axum::routing::{get, post};

    let auth_layer = middleware::from_fn_with_state(state.clone(), auth_or_localhost);

    axum::Router::new()
        .route("/v1/models", get(routes::list_models).layer(auth_layer.clone()))
        .route("/v1/embeddings", post(routes::create_embeddings).layer(auth_layer.clone()))
        .route(
            "/v1/chat/completions",
            post(routes::create_chat_completions).layer(auth_layer.clone()),
        )
        .route("/status", get(routes::get_status).layer(auth_layer.clone()))
        .route("/health", get(health_check).layer(auth_layer.clone()))
        .route("/reload", post(handle_http_reload).layer(auth_layer.clone()))
        .route(
            "/admin/providers/{name}/offline",
            post(routes::admin_offline).layer(auth_layer.clone()),
        )
        .route(
            "/admin/providers/{name}/online",
            post(routes::admin_online).layer(auth_layer.clone()),
        )
        .route(
            "/admin/providers/{name}/reset",
            post(routes::admin_reset).layer(auth_layer.clone()),
        )
        .layer(middleware::from_fn(add_request_id))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers(Any),
        )
        .with_state(state)
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use oxllm_core::state::AppState;

    #[test]
    fn test_check_bearer_logic() {
        // expected empty → always allowed
        assert!(check_bearer(None, ""));
        assert!(check_bearer(Some("garbage"), ""));
        // non-empty expected → only exact bearer matches
        assert!(check_bearer(Some("Bearer secret123"), "secret123"));
        assert!(!check_bearer(Some("Bearer wrong"), "secret123"));
        assert!(!check_bearer(None, "secret123"));
        assert!(!check_bearer(Some("Basic abc"), "secret123"));
        assert!(!check_bearer(Some("Bearer secret1234"), "secret123"));
    }

    #[tokio::test]
    async fn test_auth_loopback_passes_without_key() {
        // Loopback always passes; verify the app serves /status without a bearer.
        let state = dummy_state("");
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .get(format!("http://{}/status", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Builds a minimal `ReloadableState` with no providers — enough for middleware tests.
    fn dummy_state(api_key: &str) -> ReloadableState {
        let app_state = Arc::new(AppState {
            providers: vec![],
            virtual_models: Default::default(),
            http_client: reqwest::Client::new(),
            upstream_timeout_secs: 5,
            api_key: api_key.to_string(),
        });
        let (tx, rx) = tokio::sync::watch::channel(app_state);
        let (telemetry_tx, _trx) = tokio::sync::mpsc::channel(1024);
        ReloadableState {
            app_state: rx,
            telemetry: TelemetryClient::new(telemetry_tx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: tx,
                source: ConfigSource::File(PathBuf::from("config.toml")),
            },
        }
    }
}
