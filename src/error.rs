use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// DynamoDB operation failed
    Dynamo(Box<dyn std::error::Error + Send + Sync>),

    /// Sitemap fetch or streaming failed
    SitemapFetch(reqwest::Error),

    /// Sitemap response exceeded size limit
    SitemapTooLarge(u64),

    /// Request body exceeded size limit
    PayloadTooLarge,

    /// Rate limit exceeded
    RateLimited,

    /// Resource not found or disabled
    NotFound,

    /// TCP bind or server startup failed
    Serve(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dynamo(e) => write!(f, "DynamoDB error: {e}"),
            Self::SitemapFetch(e) => write!(f, "sitemap fetch failed: {e}"),
            Self::SitemapTooLarge(len) => write!(f, "sitemap too large: {len} bytes"),
            Self::PayloadTooLarge => write!(f, "payload too large"),
            Self::RateLimited => write!(f, "rate limited"),
            Self::NotFound => write!(f, "not found"),
            Self::Serve(e) => write!(f, "server error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Dynamo(e) => Some(&**e),
            Self::SitemapFetch(e) => Some(e),
            Self::Serve(e) => Some(e),
            _ => None,
        }
    }
}

impl<E, R> From<aws_sdk_dynamodb::error::SdkError<E, R>> for Error
where
    E: std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    fn from(e: aws_sdk_dynamodb::error::SdkError<E, R>) -> Self {
        Self::Dynamo(Box::new(e))
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::SitemapFetch(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Serve(e)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Log server-side details, return minimal status to client
        match &self {
            Self::Dynamo(e) => tracing::error!("DynamoDB error: {e}"),
            Self::SitemapFetch(e) => tracing::warn!("sitemap fetch failed: {e}"),
            Self::SitemapTooLarge(len) => tracing::warn!("sitemap too large: {len} bytes"),
            Self::Serve(e) => tracing::error!("server error: {e}"),
            _ => {}
        }

        let status = match self {
            Self::Dynamo(_) | Self::Serve(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::SitemapFetch(_) | Self::SitemapTooLarge(_) => StatusCode::BAD_GATEWAY,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::NotFound => StatusCode::NOT_FOUND,
        };

        status.into_response()
    }
}
