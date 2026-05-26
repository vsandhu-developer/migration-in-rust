use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ErrorType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorContext {
    pub error_type: ErrorType,
    pub http_status: i32,
    pub message: String,
    pub context: String,
    pub retry_count: i32,
    pub timestamp: String,
}

impl ErrorContext {
    pub fn new(error_type: ErrorType, message: impl Into<String>) -> Self {
        Self {
            error_type,
            http_status: 0,
            message: message.into(),
            context: String::new(),
            retry_count: 0,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }

    pub fn with_status(mut self, status: i32) -> Self {
        self.http_status = status;
        self
    }

    pub fn with_context(mut self, ctx: impl Into<String>) -> Self {
        self.context = ctx.into();
        self
    }

    pub fn to_json(&self) -> Value {
        json!({
            "errorType": self.error_type.name(),
            "httpStatus": self.http_status,
            "message": self.message,
            "context": self.context,
            "retryCount": self.retry_count,
            "timestamp": self.timestamp,
        })
    }
}
