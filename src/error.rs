use serde::Serialize;
use std::fmt;

/// Deliberately excludes upstream bodies, URLs, credentials and source content.
#[derive(Debug, Clone, Serialize)]
pub struct Error {
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip)]
    pub retry_delay_ms: Option<u64>,
}
impl Error {
    pub fn new(code: &'static str) -> Self {
        Self {
            code,
            status: None,
            retry_delay_ms: None,
        }
    }
    pub fn http(status: u16) -> Self {
        Self {
            code: "http_rejected",
            status: Some(status),
            retry_delay_ms: None,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn require(ok: bool, code: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::new(code))
    }
}
