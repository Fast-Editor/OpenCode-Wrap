use std::fmt;

#[derive(Debug, Clone)]
pub struct UpstreamError {
    pub upstream_status: u16,
    pub upstream_body: String,
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "opencode backend -> {}", self.upstream_status)
    }
}

impl std::error::Error for UpstreamError {}

#[derive(Debug, Clone)]
pub struct OcoTimeoutError {
    pub method: String,
    pub path: String,
    pub timeout_ms: u64,
}

impl OcoTimeoutError {
    pub fn new(method: &str, path: &str, timeout_ms: u64) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            timeout_ms,
        }
    }
}

impl fmt::Display for OcoTimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "opencode request timeout after {}ms: {} {}",
            self.timeout_ms, self.method, self.path
        )
    }
}

impl std::error::Error for OcoTimeoutError {}

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub message: String,
    pub http_status: u16,
    pub code: String,
}

impl ValidationError {
    pub fn new(message: impl Into<String>, http_status: u16, code: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            http_status,
            code: code.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ValidationError {}

#[derive(Debug, Clone)]
pub struct ClientAbortError {
    pub message: String,
}

impl ClientAbortError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Default for ClientAbortError {
    fn default() -> Self {
        Self::new("client disconnected")
    }
}

impl fmt::Display for ClientAbortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ClientAbortError {}

#[derive(Clone, Debug)]
pub enum WrapError {
    Upstream(UpstreamError),
    Timeout(OcoTimeoutError),
    Validation(ValidationError),
    ClientAbort(ClientAbortError),
    Other(String),
}

impl fmt::Display for WrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WrapError::Upstream(u) => write!(f, "{}", u),
            WrapError::Timeout(t) => write!(f, "{}", t),
            WrapError::Validation(v) => write!(f, "{}", v),
            WrapError::ClientAbort(c) => write!(f, "{}", c),
            WrapError::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for WrapError {}

impl From<UpstreamError> for WrapError {
    fn from(e: UpstreamError) -> Self {
        WrapError::Upstream(e)
    }
}

impl From<OcoTimeoutError> for WrapError {
    fn from(e: OcoTimeoutError) -> Self {
        WrapError::Timeout(e)
    }
}

impl From<ValidationError> for WrapError {
    fn from(e: ValidationError) -> Self {
        WrapError::Validation(e)
    }
}

impl From<ClientAbortError> for WrapError {
    fn from(e: ClientAbortError) -> Self {
        WrapError::ClientAbort(e)
    }
}
