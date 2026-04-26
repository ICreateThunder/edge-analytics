use crate::error::{Error, Result};
use futures_util::StreamExt;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashSet;

/// Maximum sitemap response size (1MB)
const MAX_SITEMAP_BYTES: u64 = 1_048_576;
/// Maximum paths accepted from a single sitemap
const MAX_SITEMAP_PATHS: usize = 500;

/// Fetch sitemap and extract paths as a HashSet.
///
/// Uses a pre-configured client with redirects disabled and streams the
/// response with a size cap to prevent memory exhaustion. Parses XML
/// properly via quick-xml to avoid injection via comments or CDATA.
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

    Ok(parse_sitemap(&body_bytes, site_origin))
}

/// Parse sitemap XML and extract paths under the given origin.
fn parse_sitemap(xml: &[u8], site_origin: &str) -> HashSet<String> {
    let mut reader = Reader::from_reader(xml);
    let mut paths = HashSet::new();
    let mut inside_loc = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"loc" => {
                inside_loc = true;
            }
            Ok(Event::Text(e)) if inside_loc => {
                if paths.len() >= MAX_SITEMAP_PATHS {
                    break;
                }
                if let Ok(url) = e.unescape() {
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
                inside_loc = false;
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"loc" => {
                inside_loc = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                tracing::warn!("sitemap XML parse error: {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    paths
}

pub(crate) fn is_valid_path(path: &str) -> bool {
    path == "/"
        || (path.starts_with('/')
            && path.len() <= 128
            && !path.contains("//")
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
        assert!(!is_valid_path("//double-slash"));
        assert!(!is_valid_path("/foo//bar"));
    }

    #[test]
    fn parse_standard_sitemap() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/</loc></url>
  <url><loc>https://example.com/about</loc></url>
  <url><loc>https://example.com/projects/test</loc></url>
</urlset>"#;
        let paths = parse_sitemap(xml, "https://example.com");
        assert_eq!(paths.len(), 3);
        assert!(paths.contains("/"));
        assert!(paths.contains("/about"));
        assert!(paths.contains("/projects/test"));
    }

    #[test]
    fn parse_ignores_comments_and_cdata() {
        let xml = br#"<?xml version="1.0"?>
<urlset>
  <!-- <loc>https://example.com/hidden</loc> -->
  <url><loc>https://example.com/real</loc></url>
  <url><loc><![CDATA[https://example.com/cdata]]></loc></url>
</urlset>"#;
        let paths = parse_sitemap(xml, "https://example.com");
        assert!(paths.contains("/real"));
        // CDATA and comments are both ignored — only plain text inside <loc> is accepted
        assert!(!paths.contains("/cdata"));
        assert!(!paths.contains("/hidden"));
    }

    #[test]
    fn parse_rejects_invalid_paths() {
        let xml = br#"<urlset>
  <url><loc>https://example.com/valid</loc></url>
  <url><loc>https://example.com/&lt;script&gt;</loc></url>
  <url><loc>https://other.com/wrong-origin</loc></url>
</urlset>"#;
        let paths = parse_sitemap(xml, "https://example.com");
        assert_eq!(paths.len(), 1);
        assert!(paths.contains("/valid"));
    }

    #[test]
    fn parse_caps_at_max_paths() {
        let mut xml = String::from("<urlset>");
        for i in 0..600 {
            xml.push_str(&format!(
                "<url><loc>https://example.com/page-{i}</loc></url>"
            ));
        }
        xml.push_str("</urlset>");
        let paths = parse_sitemap(xml.as_bytes(), "https://example.com");
        assert_eq!(paths.len(), MAX_SITEMAP_PATHS);
    }
}
