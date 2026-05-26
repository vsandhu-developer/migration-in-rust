use super::ErrorType;

pub struct ErrorClassifier;

impl ErrorClassifier {
    /// Classify based on HTTP status code.
    pub fn classify_http(status: u16) -> ErrorType {
        match status {
            408 => ErrorType::NetworkTimeout,
            429 => ErrorType::RateLimit,
            500..=599 => ErrorType::ServerError,
            400..=499 => ErrorType::ClientErrorInvalid,
            _ => ErrorType::UnknownError,
        }
    }

    /// Classify based on an error message string (keyword match).
    pub fn classify_message(message: &str) -> ErrorType {
        let lower = message.to_lowercase();
        if lower.contains("timeout") || lower.contains("timed out") {
            ErrorType::NetworkTimeout
        } else if lower.contains("econnrefused") || lower.contains("connection refused") {
            ErrorType::NetworkTimeout
        } else if lower.contains("rate limit") || lower.contains("too many requests") {
            ErrorType::RateLimit
        } else if lower.contains("parse") || lower.contains("json") || lower.contains("malformed") {
            ErrorType::ParseError
        } else if lower.contains("missing") || lower.contains("required") || lower.contains("invalid") {
            ErrorType::ValidationError
        } else {
            ErrorType::UnknownError
        }
    }

    pub fn classify_anyhow(err: &anyhow::Error) -> ErrorType {
        let mut chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>().join(" | ");
        if chain.is_empty() {
            chain = err.to_string();
        }
        Self::classify_message(&chain)
    }

    pub fn is_retryable(error_type: ErrorType) -> bool {
        matches!(
            error_type,
            ErrorType::NetworkTimeout
                | ErrorType::RateLimit
                | ErrorType::ServerError
                | ErrorType::UnknownError
                | ErrorType::ImageUploadError
                | ErrorType::ArticlePostError
        )
    }

    pub fn max_retries(error_type: ErrorType) -> u32 {
        if Self::is_retryable(error_type) {
            3
        } else {
            0
        }
    }
}
