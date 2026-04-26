use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::scan::ScanError;
use aws_sdk_dynamodb::operation::update_item::UpdateItemError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub(crate) enum Error {
    DynamoScan(SdkError<ScanError>),
    DynamoUpdate(SdkError<UpdateItemError>),
    SitemapFetch(reqwest::Error),
    SitemapTooLarge(u64),
    PayloadTooLarge,
    RateLimited,
    NotFound,
    StatsUnavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DynamoScan(e) => write!(f, "DynamoDB scan failed: {e}"),
            Self::DynamoUpdate(e) => write!(f, "DynamoDB update failed: {e}"),
            Self::SitemapFetch(e) => write!(f, "sitemap fetch failed: {e}"),
            Self::SitemapTooLarge(len) => write!(f, "sitemap too large: {len} bytes"),
            Self::PayloadTooLarge => write!(f, "payload too large"),
            Self::RateLimited => write!(f, "rate limited"),
            Self::NotFound => write!(f, "not found"),
            Self::StatsUnavailable => write!(f, "stats temporarily unavailable"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DynamoScan(e) => Some(e),
            Self::DynamoUpdate(e) => Some(e),
            Self::SitemapFetch(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SdkError<ScanError>> for Error {
    fn from(e: SdkError<ScanError>) -> Self {
        Self::DynamoScan(e)
    }
}

impl From<SdkError<UpdateItemError>> for Error {
    fn from(e: SdkError<UpdateItemError>) -> Self {
        Self::DynamoUpdate(e)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::SitemapFetch(e)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match &self {
            Self::DynamoScan(e) => tracing::error!("DynamoDB scan failed: {e}"),
            Self::DynamoUpdate(e) => tracing::error!("DynamoDB update failed: {e}"),
            Self::SitemapFetch(e) => tracing::warn!("sitemap fetch failed: {e}"),
            Self::SitemapTooLarge(len) => tracing::warn!("sitemap too large: {len} bytes"),
            _ => {}
        }

        let status = match self {
            Self::DynamoScan(_) | Self::DynamoUpdate(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::SitemapFetch(_) | Self::SitemapTooLarge(_) => StatusCode::BAD_GATEWAY,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::StatsUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };

        status.into_response()
    }
}
