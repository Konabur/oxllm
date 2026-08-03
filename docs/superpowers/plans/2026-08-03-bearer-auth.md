# Bearer API Key Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add optional Bearer token authentication to the public API endpoints of the `oxllm` proxy (`/v1/chat/completions`, `/v1/embeddings`, `/v1/models`), configured via `server.api_key` in `config.toml` (with env var expansion).

**Architecture:** Extend `ServerConfig` with `api_key: Option<String>`, propagate it to `AppState`, and implement an Axum middleware (`require_bearer_auth`) that validates `Authorization: Bearer <key>` using constant-time comparison when enabled.

**Tech Stack:** Rust, Axum 0.8, Tower-http, Tokio.

## Global Constraints
- Pure-Rust TLS & compatibility with existing config validation.
- Zero unwraps in user-facing paths.
- Constant-time string comparison for keys (or standard secure equality check) to prevent timing attacks.

---

### Task 1: Add `api_key` to `ServerConfig` and validation

**Files:**
- Modify: `crates/oxllm-core/src/config.rs`
- Test: `crates/oxllm-core/src/config.rs` (inline tests)

**Interfaces:**
- Produces: `ServerConfig::api_key: Option<String>`

- [ ] **Step 1: Write the test for server api_key loading**

```rust
    #[test]
    fn test_server_api_key_parsing() {
        std::env::set_var("TEST_PROXY_KEY", "secret-123");
        let input = r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = "http://127.0.0.1:4318"
            api_key = "${TEST_PROXY_KEY}"

            [[providers]]
            name = "p1"
            enabled = true
            base_url = "https://api.test.com/v1/"
            api_key = "key"
            models = ["m1"]
        "#;
        let expanded = expand_env_vars(input).unwrap();
        let config: Config = toml::from_str(&expanded).unwrap();
        assert_eq!(config.server.api_key.as_deref(), Some("secret-123"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib -- --nocapture`
Expected: FAIL because `api_key` is not a field on `ServerConfig`.

- [ ] **Step 3: Update `ServerConfig` struct in `crates/oxllm-core/src/config.rs`**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub otel_endpoint: String,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default = "default_bind_family")]
    pub bind_family: String,
    #[serde(default)]
    pub api_key: Option<String>,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/oxllm-core/src/config.rs
git commit -m "feat(core): add optional api_key to ServerConfig"
```

---

### Task 2: Propagate `api_key` to `AppState`

**Files:**
- Modify: `crates/oxllm/src/main.rs`
- Test: `crates/oxllm/src/main.rs` (integration tests)

**Interfaces:**
- Consumes: `config.server.api_key`
- Produces: `AppState::api_key: Option<String>`

- [ ] **Step 1: Add `api_key` field to `AppState` struct in `crates/oxllm-core/src/state.rs`**

Let's check `crates/oxllm-core/src/state.rs` first to be precise.
Modify: `crates/oxllm-core/src/state.rs`

```rust
pub struct AppState {
    pub providers: Vec<ProviderState>,
    pub virtual_models: HashMap<String, Vec<VirtualModelTarget>>,
    pub http_client: reqwest::Client,
    pub upstream_timeout_secs: u64,
    pub api_key: Option<String>,
}
```

- [ ] **Step 2: Populate `api_key` in `build_app_state` in `crates/oxllm/src/main.rs`**

```rust
    Ok(AppState {
        providers,
        virtual_models: config.virtual_models,
        http_client,
        upstream_timeout_secs: config.server.upstream_timeout_secs,
        api_key: config.server.api_key,
    })
```

- [ ] **Step 3: Run compilation check**

Run: `cargo check`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/oxllm-core/src/state.rs crates/oxllm/src/main.rs
git commit -m "feat: propagate api_key into AppState"
```

---

### Task 3: Implement `require_bearer_auth` middleware and mount on public routes

**Files:**
- Modify: `crates/oxllm/src/main.rs`
- Test: `crates/oxllm/src/main.rs` (integration tests)

**Interfaces:**
- Consumes: `Arc<AppState>` via Axum State/Extension or `FromRef`
- Produces: Axum middleware `require_bearer_auth`

- [ ] **Step 1: Implement `require_bearer_auth` middleware function in `crates/oxllm/src/main.rs`**

```rust
async fn require_bearer_auth(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    if let Some(ref expected_key) = state.api_key {
        let auth_header = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok());
        let authorized = match auth_header {
            Some(val) if val.starts_with("Bearer ") => {
                let token = &val["Bearer ".len()..];
                // Constant-time comparison
                subtle_ct_eq(token.as_bytes(), expected_key.as_bytes())
            },
            _ => false,
        };

        if !authorized {
            warn!(target: "oxllm::security", "Unauthorized access attempt to public API endpoint");
            let body = serde_json::json!({
                "error": {
                    "message": "Unauthorized: invalid or missing Bearer token",
                    "type": "invalid_request_error",
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
            return Err(response);
        }
    }
    Ok(next.run(req).await)
}

fn subtle_ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
```

- [ ] **Step 2: Mount `require_bearer_auth` on public routes in `run_serve` in `crates/oxllm/src/main.rs`**

```rust
    let app = axum::Router::new()
        .route("/v1/models", get(routes::list_models))
        .route("/v1/embeddings", post(routes::create_embeddings))
        .route(
            "/v1/chat/completions",
            post(routes::create_chat_completions),
        )
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            require_bearer_auth,
        ))
...
```

Note: Since `ReloadableState` holds a watch channel to `Arc<AppState>`, we can use `axum::middleware::from_fn_with_state` or extract `Arc<AppState>` via `FromRef`. Let's check how other states are extracted. `Arc<AppState>` can be extracted from `ReloadableState` via `FromRef`. So we can use `.layer(middleware::from_fn_with_state(reloadable_state.clone(), require_bearer_auth))` or simply `.layer(middleware::from_fn(require_bearer_auth))` since Axum extracts `Arc<AppState>` automatically if `AppState` implements `FromRef<ReloadableState>` (which it does via line 156 of `main.rs`).

Let's use `middleware::from_fn(require_bearer_auth)` with `State<Arc<AppState>>`.

- [ ] **Step 3: Add integration tests for Bearer auth success and failure**

Add to `integration_tests` module in `crates/oxllm/src/main.rs`:

```rust
    #[tokio::test]
    async fn test_integration_bearer_auth() {
        let p1 = ProviderState {
            name: "prov1".to_string(),
            base_url: Url::parse("http://localhost:1234/v1/").unwrap(),
            api_key: "key1".to_string(),
            models: vec!["model".to_string()],
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
        };

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "model".to_string(),
            vec![VirtualModelTarget {
                provider: "prov1".to_string(),
                model: "model".to_string(),
            }],
        );

        let app_state = Arc::new(AppState {
            providers: vec![p1],
            virtual_models,
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
            api_key: Some("secret-proxy-token".to_string()),
        });

        let (_ws, wr) = tokio::sync::watch::channel(app_state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let state = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
            },
        };

        let router = axum::Router::new()
            .route(
                "/v1/models",
                axum::routing::get(routes::list_models),
            )
            .layer(middleware::from_fn(require_bearer_auth))
            .layer(middleware::from_fn(add_request_id))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = reqwest::Client::new();

        // 1. Request without auth -> 401
        let res = client
            .get(format!("http://{}/v1/models", proxy_addr))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        // 2. Request with wrong auth -> 401
        let res = client
            .get(format!("http://{}/v1/models", proxy_addr))
            .header("Authorization", "Bearer wrong-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);

        // 3. Request with correct auth -> 200
        let res = client
            .get(format!("http://{}/v1/models", proxy_addr))
            .header("Authorization", "Bearer secret-proxy-token")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }
```

- [ ] **Step 4: Run tests and verify they pass**

Run: `cargo test`
Expected: PASS

- [ ] **Step 5: Run strict clippy and fmt checks**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/oxllm/src/main.rs
git commit -m "feat: implement require_bearer_auth middleware and tests"
```
