use std::env;

use crate::error::ValidationError;

#[derive(Clone, Debug)]
pub struct Config {
    pub wrap_port: u16,
    pub opencode_port: u16,
    pub opencode_base: String,
    pub default_model: String,
    pub default_provider: String,
    pub wrap_cwd: String,
    pub oco_timeout_ms: u64,
    pub wrap_max_body_bytes: usize,
    pub zen_models_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self, ValidationError> {
        let opencode_port = num_env("OPENCODE_PORT", 4100, 1, 65535)? as u16;
        let opencode_base = normalize_base(
            &env::var("OPENCODE_BASE")
                .unwrap_or_else(|_| format!("http://127.0.0.1:{}", opencode_port)),
        );
        if !opencode_base.starts_with("http://") && !opencode_base.starts_with("https://") {
            return Err(ValidationError::new(
                format!(
                    "invalid OPENCODE_BASE={:?} (must start with http:// or https://)",
                    opencode_base
                ),
                500,
                "config_error",
            ));
        }
        Ok(Self {
            wrap_port: num_env("WRAP_PORT", 8000, 1, 65535)? as u16,
            opencode_port,
            opencode_base,
            default_model: env::var("WRAP_MODEL")
                .unwrap_or_else(|_| "muse-spark-1.3-contributor-free".into()),
            default_provider: env::var("WRAP_PROVIDER").unwrap_or_else(|_| "opencode".into()),
            wrap_cwd: env::var("WRAP_CWD").unwrap_or_else(|_| "/tmp".into()),
            oco_timeout_ms: num_env("WRAP_OCO_TIMEOUT_MS", 180_000, 100, 1_800_000)?,
            wrap_max_body_bytes: num_env(
                "WRAP_MAX_BODY_BYTES",
                8 * 1024 * 1024,
                1024,
                64 * 1024 * 1024,
            )? as usize,
            zen_models_url: env::var("WRAP_ZEN_MODELS_URL")
                .unwrap_or_else(|_| "https://opencode.ai/zen/v1/models".into()),
        })
    }
}

pub fn num_env(name: &str, def: u64, min: u64, max: u64) -> Result<u64, ValidationError> {
    let raw = env::var(name).ok();
    let raw = match raw {
        Some(s) if s.is_empty() => None,
        other => other,
    };
    let n = match raw {
        None => def,
        Some(s) => {
            let n = s.parse::<u64>().map_err(|_| {
                ValidationError::new(
                    format!(
                        "invalid {}={:?} (expected number {}..{})",
                        name, s, min, max
                    ),
                    500,
                    "config_error",
                )
            })?;
            if n < min || n > max {
                return Err(ValidationError::new(
                    format!(
                        "invalid {}={:?} (expected number {}..{})",
                        name, s, min, max
                    ),
                    500,
                    "config_error",
                ));
            }
            n
        }
    };
    Ok(n)
}

pub fn normalize_base(u: &str) -> String {
    u.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_base_strips_slashes() {
        assert_eq!(
            normalize_base("http://127.0.0.1:4100/"),
            "http://127.0.0.1:4100"
        );
        assert_eq!(
            normalize_base("http://127.0.0.1:4100///"),
            "http://127.0.0.1:4100"
        );
    }
}
