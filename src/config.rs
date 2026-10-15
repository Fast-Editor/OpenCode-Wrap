use std::env;

use crate::error::ValidationError;

#[derive(Clone, Debug)]
pub struct Config {
    pub wrap_port: u16,
    pub opencode_port: u16,
    pub opencode_base: String,
    /// HTTP basic-auth username for `opencode serve` (OpenCode 2 requires auth).
    pub opencode_username: String,
    /// HTTP basic-auth password. Taken from `OPENCODE_SERVER_PASSWORD`, or
    /// generated at startup and handed to the spawned `opencode serve`.
    pub opencode_password: String,
    pub opencode_password_generated: bool,
    pub default_model: String,
    pub default_provider: String,
    /// OpenCode agent the per-request session runs under.
    pub agent: String,
    pub wrap_cwd: String,
    pub oco_timeout_ms: u64,
    pub wrap_max_body_bytes: usize,
    pub zen_models_url: String,
    pub wrap_api_key: Option<String>,
    pub cors_origins: Vec<String>,
    pub max_in_flight: usize,
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
        let wrap_cwd = require_dir(&env::var("WRAP_CWD").unwrap_or_else(|_| default_wrap_cwd()))?;
        if let Ok(home) = env::var("HOME").or_else(|_| env::var("USERPROFILE")) {
            if paths_equal(&wrap_cwd, &home) {
                tracing::warn!(
                    "[wrap] WRAP_CWD is the home directory; opencode tools run there. Point WRAP_CWD at an empty sandbox directory."
                );
            }
        }
        let (opencode_password, opencode_password_generated) =
            match non_empty_env("OPENCODE_SERVER_PASSWORD") {
                Some(p) => (p, false),
                None => (random_password(), true),
            };
        Ok(Self {
            wrap_port: num_env("WRAP_PORT", 8000, 1, 65535)? as u16,
            opencode_port,
            opencode_base,
            opencode_username: non_empty_env("OPENCODE_SERVER_USERNAME")
                .unwrap_or_else(|| "opencode".into()),
            opencode_password,
            opencode_password_generated,
            default_model: env::var("WRAP_MODEL")
                .unwrap_or_else(|_| "muse-spark-1.3-contributor-free".into()),
            default_provider: env::var("WRAP_PROVIDER").unwrap_or_else(|_| "opencode".into()),
            agent: non_empty_env("WRAP_AGENT").unwrap_or_else(|| "build".into()),
            wrap_cwd,
            oco_timeout_ms: num_env("WRAP_OCO_TIMEOUT_MS", 180_000, 100, 1_800_000)?,
            wrap_max_body_bytes: num_env(
                "WRAP_MAX_BODY_BYTES",
                8 * 1024 * 1024,
                1024,
                64 * 1024 * 1024,
            )? as usize,
            zen_models_url: env::var("WRAP_ZEN_MODELS_URL")
                .unwrap_or_else(|_| "https://opencode.ai/zen/v1/models".into()),
            wrap_api_key: non_empty_env("WRAP_API_KEY"),
            cors_origins: parse_cors_origins(),
            max_in_flight: num_env("WRAP_MAX_IN_FLIGHT", 8, 1, 1024)? as usize,
        })
    }

    /// Fixture for unit tests: points at an unreachable backend and Zen URL.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            wrap_port: 8000,
            opencode_port: 4100,
            opencode_base: "http://127.0.0.1:4100".into(),
            opencode_username: "opencode".into(),
            opencode_password: "test-password".into(),
            opencode_password_generated: true,
            default_model: "muse-spark-1.3-contributor-free".into(),
            default_provider: "opencode".into(),
            agent: "build".into(),
            wrap_cwd: "/tmp".into(),
            oco_timeout_ms: 180_000,
            wrap_max_body_bytes: 8 * 1024 * 1024,
            zen_models_url: "http://127.0.0.1:1/nope".into(),
            wrap_api_key: None,
            cors_origins: Vec::new(),
            max_in_flight: 8,
        }
    }
}

/// `/tmp` on Unix; the user's temp directory (`%TEMP%`, always present) on
/// Windows, where `/tmp` does not exist and would fail `require_dir`.
fn default_wrap_cwd() -> String {
    if cfg!(windows) {
        env::temp_dir().to_string_lossy().into_owned()
    } else {
        "/tmp".into()
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn random_password() -> String {
    let bytes: [u8; 24] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn parse_cors_origins() -> Vec<String> {
    parse_cors_origins_from(std::env::var("WRAP_CORS_ORIGINS").ok().as_deref())
}

pub(crate) fn parse_cors_origins_from(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn num_env(name: &str, def: u64, min: u64, max: u64) -> Result<u64, ValidationError> {
    let raw = env::var(name).ok();
    parse_bounded_u64(raw.as_deref(), name, def, min, max)
}

pub(crate) fn parse_bounded_u64(
    raw: Option<&str>,
    name: &str,
    def: u64,
    min: u64,
    max: u64,
) -> Result<u64, ValidationError> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty());
    let Some(s) = raw else {
        return Ok(def);
    };
    let n = s.parse::<u64>().map_err(|_| {
        ValidationError::new(
            format!("invalid {name}={s:?} (expected number {min}..{max})"),
            500,
            "config_error",
        )
    })?;
    if n < min || n > max {
        return Err(ValidationError::new(
            format!("invalid {name}={s:?} (expected number {min}..{max})"),
            500,
            "config_error",
        ));
    }
    Ok(n)
}

fn require_dir(path: &str) -> Result<String, ValidationError> {
    let path = path.trim();
    if path.is_empty() || !std::path::Path::new(path).is_dir() {
        return Err(ValidationError::new(
            format!("WRAP_CWD={path:?} is not a directory"),
            500,
            "config_error",
        ));
    }
    Ok(path.to_string())
}

fn paths_equal(a: &str, b: &str) -> bool {
    std::path::Path::new(a.trim_end_matches('/')) == std::path::Path::new(b.trim_end_matches('/'))
}

pub fn normalize_base(u: &str) -> String {
    u.trim().trim_end_matches('/').to_string()
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
        assert_eq!(
            normalize_base("  http://127.0.0.1:4100/ "),
            "http://127.0.0.1:4100"
        );
    }

    #[test]
    fn parse_bounded_u64_trims_blank_and_whitespace() {
        assert_eq!(parse_bounded_u64(Some("  "), "X", 5, 1, 10).unwrap(), 5);
        assert_eq!(parse_bounded_u64(Some(""), "X", 5, 1, 10).unwrap(), 5);
        assert_eq!(parse_bounded_u64(Some(" 7 "), "X", 5, 1, 10).unwrap(), 7);
        assert!(parse_bounded_u64(Some("nope"), "X", 5, 1, 10).is_err());
    }

    #[test]
    fn require_dir_rejects_missing_path() {
        let err = require_dir("/no/such/opencode-wrap-cwd").unwrap_err();
        assert!(err.message.contains("not a directory"));
    }

    #[test]
    fn cors_origins_parsing() {
        assert!(parse_cors_origins_from(None).is_empty());
        assert!(parse_cors_origins_from(Some("")).is_empty());
        assert_eq!(
            parse_cors_origins_from(Some("https://a.example, https://b.example")),
            vec!["https://a.example", "https://b.example"]
        );
    }

    #[test]
    fn default_wrap_cwd_exists() {
        let d = default_wrap_cwd();
        assert!(std::path::Path::new(&d).is_dir(), "{d} should exist");
    }

    #[test]
    fn random_password_is_hex_and_unique() {
        let a = random_password();
        let b = random_password();
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
