use oxllm::{build_reloadable_state, build_router, ConfigSource};
use oxllm_core::config::Config;
use shuttle_axum::ShuttleAxum;

/// Shuttle entry point. Configuration comes entirely from env:
/// `OXLLM_CONFIG_TOML` (TOML string) or `OXLLM_CONFIG` (file path, local dev).
#[shuttle_runtime::main]
async fn main() -> ShuttleAxum {
    let config = Config::from_env()
        .map_err(|e| shuttle_runtime::Error::Custom(anyhow::anyhow!("config: {e}")))?;
    let source = ConfigSource::Env;
    let state = build_reloadable_state(config, source)
        .map_err(|e| shuttle_runtime::Error::Custom(anyhow::anyhow!("state: {e}")))?;
    Ok(build_router(state).into())
}
