use crate::error::{Error, Result};
use futures_util::StreamExt;
use std::collections::HashSet;

/// Maximum sitemap response size (1MB)
const MAX_SITEMAP_BYTES: u64 = 1_048_576;
/// Maximum paths accepted from a single sitemap
const MAX_SITEMAP_PATHS: usize = 500;

/// Fetch sitemap and extract paths as a HashSet.
///
/// Uses a pre-configured client with redirects disabled and streams the
/// response with a size cap to prevent memory exhaustion.
pub(crate) async fn fetch_paths(
    client: &reqwest::Client,
    sitemap_url: &str,
    site_origin: &str,
) -> Result<HashSet<String>> {
    let resp = client.get(sitemap_url).send().await?;

    if !resp.status().is_success() {
        tracing::warn!("sitemap fetch returned status {}", resp.status());
        return Ok(HashSet::new());
    }

    // Early rejection via Content-Length before buffering the body
    let content_length = resp.content_length();
    if let Some(len) = content_length {
        if len > MAX_SITEMAP_BYTES {
            return Err(Error::SitemapTooLarge(len));
        }
    }

    // Pre-allocate based on Content-Length hint, capped to MAX_SITEMAP_BYTES
    let capacity = content_length
        .map(|len| len as usize)
        .unwrap_or(8192)
        .min(MAX_SITEMAP_BYTES as usize);
    let mut body_bytes = Vec::with_capacity(capacity);

    // Stream body with a hard cap — prevents slow-drip attacks that omit Content-Length
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        body_bytes.extend_from_slice(&chunk);
        if body_bytes.len() as u64 > MAX_SITEMAP_BYTES {
            return Err(Error::SitemapTooLarge(body_bytes.len() as u64));
        }
    }

    let body = String::from_utf8_lossy(&body_bytes);
    let mut paths = HashSet::new();

    for segment in body.split("<loc>") {
        if paths.len() >= MAX_SITEMAP_PATHS {
            break;
        }
        if let Some(url) = segment.split("</loc>").next() {
            if let Some(path) = url.strip_prefix(site_origin) {
                let normalized = path.trim_end_matches('/');
                let normalized = if normalized.is_empty() {
                    "/"
                } else {
                    normalized
                };

                if is_valid_path(normalized) {
                    paths.insert(normalized.to_string());
                }
            }
        }
    }

    Ok(paths)
}

pub(crate) fn is_valid_path(path: &str) -> bool {
    path == "/"
        || (path.starts_with('/')
            && path.len() <= 128
            && path
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_paths() {
        assert!(is_valid_path("/"));
        assert!(is_valid_path("/profile"));
        assert!(is_valid_path("/projects/oxiflight"));
        assert!(is_valid_path("/projects/my-project-123"));
    }

    #[test]
    fn invalid_paths() {
        assert!(!is_valid_path(""));
        assert!(!is_valid_path("no-leading-slash"));
        assert!(!is_valid_path("/<script>"));
        assert!(!is_valid_path("/path with spaces"));
        assert!(!is_valid_path("/path?query=1"));
        assert!(!is_valid_path("/../../etc/passwd"));
        assert!(!is_valid_path(&format!("/{}", "a".repeat(128))));
    }
}
