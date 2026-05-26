use serde::{Deserialize, Serialize};

/// The 11 categories of error the system distinguishes. See ROADMAP.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorType {
    NetworkTimeout,
    RateLimit,
    ServerError,
    ClientErrorInvalid,
    ParseError,
    ValidationError,
    ImageDownloadError,
    ImageUploadError,
    ArticlePostError,
    CommentPostError,
    UnknownError,
}

impl ErrorType {
    pub fn name(self) -> &'static str {
        match self {
            ErrorType::NetworkTimeout => "NETWORK_TIMEOUT",
            ErrorType::RateLimit => "RATE_LIMIT",
            ErrorType::ServerError => "SERVER_ERROR",
            ErrorType::ClientErrorInvalid => "CLIENT_ERROR_INVALID",
            ErrorType::ParseError => "PARSE_ERROR",
            ErrorType::ValidationError => "VALIDATION_ERROR",
            ErrorType::ImageDownloadError => "IMAGE_DOWNLOAD_ERROR",
            ErrorType::ImageUploadError => "IMAGE_UPLOAD_ERROR",
            ErrorType::ArticlePostError => "ARTICLE_POST_ERROR",
            ErrorType::CommentPostError => "COMMENT_POST_ERROR",
            ErrorType::UnknownError => "UNKNOWN_ERROR",
        }
    }
}
