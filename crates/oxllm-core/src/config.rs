use crate::error::{OxllmError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub otel_endpoint: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default = "default_bind_family")]
    pub bind_family: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualModelTarget {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub providers: Vec<ProviderConfig>,
    pub virtual_models: HashMap<String, Vec<VirtualModelTarget>>,
}

impl Config {
    /// Loads a TOML configuration file and expands environment variables of style `${VAR_NAME}`.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)
            .map_err(|e| OxllmError::ConfigLoad(format!("Failed to read config file: {}", e)))?;

        let expanded = expand_env_vars(&content)?;
        let config: Config = toml::from_str(&expanded)?;
        Ok(config)
    }

    /// Builds a Config from the environment.
    ///
    /// Source precedence:
    /// 1. `OXLLM_CONFIG_TOML` — full TOML string; `${VAR}` expansion applied
    ///    first, then parsed (same order as `load_from_file`).
    /// 2. `OXLLM_CONFIG` — path to a TOML file (delegates to `load_from_file`).
    ///
    /// On Shuttle there is no filesystem config, so relying on `OXLLM_CONFIG`
    /// will surface a clear file-not-found error rather than booting silently.
    pub fn from_env() -> Result<Self> {
        if let Ok(toml_str) = std::env::var("OXLLM_CONFIG_TOML") {
            let expanded = expand_env_vars(&toml_str)?;
            let config: Config = toml::from_str(&expanded)?;
            return Ok(config);
        }
        let path = std::env::var("OXLLM_CONFIG").map_err(|_| {
            OxllmError::ConfigLoad(
                "Neither OXLLM_CONFIG_TOML nor OXLLM_CONFIG is set".into(),
            )
        })?;
        Self::load_from_file(&path)
    }

    /// Validates the configuration syntax and cross-references virtual models with defined providers.
    pub fn validate(&self) -> Result<()> {
        let provider_map: HashMap<&str, &ProviderConfig> = self
            .providers
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();

        // 1. Validate that at least one provider is configured
        if self.providers.is_empty() {
            return Err(OxllmError::ConfigLoad(
                "At least one provider must be defined".into(),
            ));
        }

        // 2. Validate that virtual models target existing, enabled providers
        for (vm_name, targets) in &self.virtual_models {
            if targets.is_empty() {
                return Err(OxllmError::ConfigLoad(format!(
                    "Virtual model '{}' has no targets configured",
                    vm_name
                )));
            }

            for target in targets {
                match provider_map.get(target.provider.as_str()) {
                    Some(provider) => {
                        if !provider.enabled {
                            // Warn or skip: we allow referencing disabled providers,
                            // but the virtual model resolution loop will bypass them.
                        }
                    },
                    None => {
                        return Err(OxllmError::ConfigLoad(format!(
                            "Virtual model '{}' targets undefined provider '{}'",
                            vm_name, target.provider
                        )));
                    },
                }
            }
        }

        Ok(())
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_upstream_timeout() -> u64 {
    5
}

fn default_bind_family() -> String {
    "ipv4".to_string()
}

/// Helper function to perform Unix shell-style `${VAR_NAME}` environment variable expansions.
#[allow(clippy::while_let_on_iterator)]
pub fn expand_env_vars(raw_content: &str) -> Result<String> {
    let mut expanded = String::new();
    let mut chars = raw_content.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '$' {
            if let Some(&(_, '{')) = chars.peek() {
                chars.next(); // Consume '{'

                let mut var_name = String::new();
                let mut found_close = false;

                while let Some((_, var_ch)) = chars.next() {
                    if var_ch == '}' {
                        found_close = true;
                        break;
                    }
                    var_name.push(var_ch);
                }

                if !found_close {
                    return Err(OxllmError::ConfigLoad(format!(
                        "Unclosed environment variable placeholder starting at index {}",
                        idx
                    )));
                }

                // Strictly resolve environment variable
                let val = std::env::var(&var_name)
                    .map_err(|_| OxllmError::EnvVarMissing(var_name.clone()))?;

                expanded.push_str(&val);
                continue;
            }
        }
        expanded.push(ch);
    }

    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars_success() {
        std::env::set_var("TEST_HOST", "127.0.0.1");
        std::env::set_var("TEST_KEY", "groq-key-123");

        let input = r#"
            host = "${TEST_HOST}"
            api_key = "${TEST_KEY}"
            other = "literal $10 string"
        "#;

        let result = expand_env_vars(input).unwrap();
        assert!(result.contains("host = \"127.0.0.1\""));
        assert!(result.contains("api_key = \"groq-key-123\""));
        assert!(result.contains("other = \"literal $10 string\""));
    }

    #[test]
    fn test_expand_env_vars_missing() {
        std::env::remove_var("MISSING_VAR_XYZ");
        let input = r#"api_key = "${MISSING_VAR_XYZ}""#;
        let result = expand_env_vars(input);
        assert!(
            matches!(result, Err(OxllmError::EnvVarMissing(ref name)) if name == "MISSING_VAR_XYZ")
        );
    }

    #[test]
    fn test_expand_env_vars_unclosed() {
        let input = r#"api_key = "${UNCLOSED"#;
        let result = expand_env_vars(input);
        assert!(matches!(result, Err(OxllmError::ConfigLoad(_))));
    }

    #[test]
    fn test_server_config_defaults() {
        let toml_str = r#"
            providers = []

            [server]

            [virtual_models]
        "#;
        // host/port/otel_endpoint/api_key/bind_family must default when absent
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.server.otel_endpoint, "");
        assert_eq!(cfg.server.api_key, "");
        assert_eq!(cfg.server.upstream_timeout_secs, 5);
        assert_eq!(cfg.server.bind_family, "ipv4");
    }

    #[test]
    fn test_server_config_parse_api_key() {
        let toml_str = r#"
            providers = []

            [server]
            api_key = "sk-test-123"

            [virtual_models]
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.server.api_key, "sk-test-123");
    }

    #[test]
    fn test_from_env_toml_string() {
        // OXLLM_CONFIG_TOML wins over OXLLM_CONFIG
        std::env::set_var("TEST_KEY", "env-key-abc");
        std::env::set_var(
            "OXLLM_CONFIG_TOML",
            r#"
            [server]
            api_key = "${TEST_KEY}"
            [[providers]]
            name = "p1"
            enabled = true
            base_url = "https://api.example.com/v1/"
            api_key = "k"
            models = ["m"]
            [virtual_models]
            vm = [{ provider = "p1", model = "m" }]
            "#,
        );
        std::env::set_var("OXLLM_CONFIG", "/nonexistent/path.toml");

        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.server.api_key, "env-key-abc");
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.virtual_models["vm"][0].provider, "p1");

        std::env::remove_var("TEST_KEY");
        std::env::remove_var("OXLLM_CONFIG_TOML");
        std::env::remove_var("OXLLM_CONFIG");
    }

    #[test]
    fn test_from_env_no_sources() {
        std::env::remove_var("OXLLM_CONFIG_TOML");
        std::env::remove_var("OXLLM_CONFIG");
        let err = Config::from_env().unwrap_err();
        assert!(matches!(err, OxllmError::ConfigLoad(_)));
    }
}
