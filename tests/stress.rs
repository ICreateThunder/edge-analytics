use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FALLBACK_PATHS: &[&str] = &["/", "/about", "/contact"];

struct Metrics {
    success: AtomicU64,
    errors: AtomicU64,
    rate_limited: AtomicU64,
    latencies_us: std::sync::Mutex<Vec<u64>>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            success: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            latencies_us: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn record(&self, status: u16, elapsed: Duration) {
        let us = elapsed.as_micros() as u64;
        self.latencies_us.lock().unwrap().push(us);

        match status {
            200 | 204 => {
                self.success.fetch_add(1, Ordering::Relaxed);
            }
            429 => {
                self.rate_limited.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn report(&self, label: &str, wall_time: Duration) {
        let success = self.success.load(Ordering::Relaxed);
        let errors = self.errors.load(Ordering::Relaxed);
        let rate_limited = self.rate_limited.load(Ordering::Relaxed);
        let total = success + errors + rate_limited;

        let mut latencies = self.latencies_us.lock().unwrap();
        latencies.sort_unstable();

        let p50 = percentile(&latencies, 50.0);
        let p95 = percentile(&latencies, 95.0);
        let p99 = percentile(&latencies, 99.0);
        let max = latencies.last().copied().unwrap_or(0);

        let rps = if wall_time.as_secs_f64() > 0.0 {
            total as f64 / wall_time.as_secs_f64()
        } else {
            0.0
        };

        println!("\n\x1b[1;36m── {label} ──\x1b[0m");
        println!(
            "  Requests:     {total}  ({success} ok, {rate_limited} rate-limited, {errors} errors)"
        );
        println!("  Throughput:   {rps:.0} req/s");
        println!(
            "  Latency:      p50={:.2}ms  p95={:.2}ms  p99={:.2}ms  max={:.2}ms",
            p50 as f64 / 1000.0,
            p95 as f64 / 1000.0,
            p99 as f64 / 1000.0,
            max as f64 / 1000.0,
        );
        println!("  Wall time:    {:.2}s", wall_time.as_secs_f64());
    }
}

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Load paths from a sitemap URL by parsing <loc> elements.
/// Falls back to FALLBACK_PATHS if the fetch fails.
async fn load_paths(client: &reqwest::Client, sitemap_url: &str, site_origin: &str) -> Vec<String> {
    if let Ok(resp) = client.get(sitemap_url).send().await {
        if let Ok(body) = resp.text().await {
            let paths: Vec<String> = body
                .split("<loc>")
                .filter_map(|seg| seg.split("</loc>").next())
                .filter_map(|url| url.strip_prefix(site_origin))
                .map(|p| {
                    let trimmed = p.trim_end_matches('/');
                    if trimmed.is_empty() {
                        "/".to_string()
                    } else {
                        trimmed.to_string()
                    }
                })
                .collect();

            if !paths.is_empty() {
                return paths;
            }
        }
    }

    eprintln!("  Warning: could not load sitemap, using fallback paths");
    FALLBACK_PATHS.iter().map(|s| s.to_string()).collect()
}

#[tokio::main]
async fn main() {
    let base_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:3001".to_string());

    let sitemap_url =
        std::env::var("SITEMAP_URL").unwrap_or_else(|_| "http://localhost:8888/sitemap.xml".into());
    let site_origin =
        std::env::var("SITE_ORIGIN").unwrap_or_else(|_| "http://localhost:9999".into());

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(256)
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let paths = Arc::new(load_paths(&client, &sitemap_url, &site_origin).await);

    println!("\x1b[1;33m╔══════════════════════════════════════════════════╗\x1b[0m");
    println!("\x1b[1;33m║     edge-analytics stress test                  ║\x1b[0m");
    println!("\x1b[1;33m║     target: {:<36} ║\x1b[0m", base_url);
    println!(
        "\x1b[1;33m║     paths:  {:<36} ║\x1b[0m",
        format!("{} from sitemap", paths.len())
    );
    println!("\x1b[1;33m╚══════════════════════════════════════════════════╝\x1b[0m");

    // ══════════════════════════════════════════════════════════════════
    // Phase 1: Deterministic validation (run BEFORE load tests)
    // These tests depend on fresh rate limiter state.
    // ══════════════════════════════════════════════════════════════════

    // ── Test 1: Oversized body rejection ─────────────────────────────
    {
        println!("\n\x1b[1;36m── Oversized body rejection ──\x1b[0m");
        let big_body = "x".repeat(2048);
        let start = Instant::now();
        let resp = client
            .post(format!("{}/views", base_url))
            .header("content-type", "application/json")
            .body(big_body)
            .send()
            .await
            .unwrap();
        let elapsed = start.elapsed();
        let status = resp.status().as_u16();
        let ok = status == 413;
        println!(
            "  {} status={status} latency={:.2}ms",
            if ok {
                "\x1b[32mPASS\x1b[0m"
            } else {
                "\x1b[31mFAIL\x1b[0m"
            },
            elapsed.as_secs_f64() * 1000.0
        );
    }

    // ── Test 2: Invalid path rejection ───────────────────────────────
    {
        println!("\n\x1b[1;36m── Invalid path rejection ──\x1b[0m");
        let cases = vec![
            (
                r#"{"path":"/<script>alert(1)</script>"}"#,
                "XSS attempt",
                204,
            ),
            (r#"{"path":"/../../etc/passwd"}"#, "Path traversal", 204),
            (r#"{"path":"/nonexistent-page"}"#, "Unlisted path", 204),
            (r#"not json at all"#, "Malformed JSON", 204),
            // Empty path is rejected before reaching the whitelist
            (r#"{"path":""}"#, "Empty path", 204),
        ];
        for (body, label, expected) in cases {
            let start = Instant::now();
            let resp = client
                .post(format!("{}/views", base_url))
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            let elapsed = start.elapsed();
            let status = resp.status().as_u16();
            let ok = status == expected;
            println!(
                "  {} {label:<20} status={status} (expected {expected}) latency={:.2}ms",
                if ok {
                    "\x1b[32mPASS\x1b[0m"
                } else {
                    "\x1b[31mFAIL\x1b[0m"
                },
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }

    // ── Test 3: Rate limiter saturation ──────────────────────────────
    // Uses the last path in the list — least likely to be hit by
    // random selection in later load tests.
    {
        let rate_test_path = paths.last().unwrap().clone();
        println!(
            "\n\x1b[1;36m── Rate limiter saturation (single path: {rate_test_path}) ──\x1b[0m"
        );
        let mut ok_count = 0u32;
        let mut limited_count = 0u32;
        let mut error_count = 0u32;

        // Fire 120 requests rapidly to one path (limit is 60/min)
        let body = format!(r#"{{"path":"{}"}}"#, rate_test_path);
        for _ in 0..120 {
            let resp = client
                .post(format!("{}/views", base_url))
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await
                .unwrap();
            match resp.status().as_u16() {
                204 => ok_count += 1,
                429 => limited_count += 1,
                _ => error_count += 1,
            }
        }
        let triggered = limited_count > 0 && ok_count <= 60;
        println!(
            "  {} 120 rapid requests → {ok_count} accepted, {limited_count} rate-limited, {error_count} errors",
            if triggered { "\x1b[32mPASS\x1b[0m" } else { "\x1b[31mFAIL\x1b[0m" },
        );
        println!("  Rate limiter engaged at request ~{}", ok_count + 1);
    }

    // ══════════════════════════════════════════════════════════════════
    // Phase 2: Load tests (may exhaust rate limiters — that's expected)
    // ══════════════════════════════════════════════════════════════════

    // ── Test 4: Sustained throughput (POST /views) ───────────────────
    {
        let paths = Arc::clone(&paths);
        run_test(
            "Sustained load: POST /views (100 concurrent, 5s)",
            &client,
            &base_url,
            100,
            Duration::from_secs(5),
            move |client, base_url| {
                let path = paths[fastrand_usize() % paths.len()].clone();
                let body = format!(r#"{{"path":"{}"}}"#, path);
                async move {
                    let start = Instant::now();
                    let result = client
                        .post(format!("{}/views", base_url))
                        .header("content-type", "application/json")
                        .body(body)
                        .send()
                        .await;
                    (
                        result.map(|r| r.status().as_u16()).unwrap_or(0),
                        start.elapsed(),
                    )
                }
            },
        )
        .await;
    }

    // ── Test 5: GET /stats under load ────────────────────────────────
    run_test(
        "Stats endpoint: GET /stats (50 concurrent, 3s)",
        &client,
        &base_url,
        50,
        Duration::from_secs(3),
        |client, base_url| async move {
            let start = Instant::now();
            let result = client.get(format!("{}/stats", base_url)).send().await;
            (
                result.map(|r| r.status().as_u16()).unwrap_or(0),
                start.elapsed(),
            )
        },
    )
    .await;

    // ── Test 6: GET /status ──────────────────────────────────────────
    run_test(
        "Health check: GET /status (50 concurrent, 2s)",
        &client,
        &base_url,
        50,
        Duration::from_secs(2),
        |client, base_url| async move {
            let start = Instant::now();
            let result = client.get(format!("{}/status", base_url)).send().await;
            (
                result.map(|r| r.status().as_u16()).unwrap_or(0),
                start.elapsed(),
            )
        },
    )
    .await;

    // ── Test 7: Connection storm ─────────────────────────────────────
    {
        let paths = Arc::clone(&paths);
        run_test(
            "Connection storm: 2000 concurrent POST /views (2s)",
            &client,
            &base_url,
            2000,
            Duration::from_secs(2),
            move |client, base_url| {
                let path = paths[fastrand_usize() % paths.len()].clone();
                let body = format!(r#"{{"path":"{}"}}"#, path);
                async move {
                    let start = Instant::now();
                    let result = client
                        .post(format!("{}/views", base_url))
                        .header("content-type", "application/json")
                        .body(body)
                        .send()
                        .await;
                    (
                        result.map(|r| r.status().as_u16()).unwrap_or(0),
                        start.elapsed(),
                    )
                }
            },
        )
        .await;
    }

    // ── Test 8: Mixed workload ───────────────────────────────────────
    {
        let paths = Arc::clone(&paths);
        run_test(
            "Mixed workload: /views + /stats + /status (200 concurrent, 5s)",
            &client,
            &base_url,
            200,
            Duration::from_secs(5),
            move |client, base_url| {
                let paths = Arc::clone(&paths);
                async move {
                    let start = Instant::now();
                    let choice = fastrand_usize() % 10;
                    let result = match choice {
                        0 => {
                            // 10% stats
                            client.get(format!("{}/stats", base_url)).send().await
                        }
                        1 => {
                            // 10% status
                            client.get(format!("{}/status", base_url)).send().await
                        }
                        _ => {
                            // 80% views
                            let path = paths[fastrand_usize() % paths.len()].clone();
                            client
                                .post(format!("{}/views", base_url))
                                .header("content-type", "application/json")
                                .body(format!(r#"{{"path":"{}"}}"#, path))
                                .send()
                                .await
                        }
                    };
                    (
                        result.map(|r| r.status().as_u16()).unwrap_or(0),
                        start.elapsed(),
                    )
                }
            },
        )
        .await;
    }

    println!("\n\x1b[1;32m✓ Stress test complete\x1b[0m\n");
}

async fn run_test<F, Fut>(
    label: &str,
    client: &reqwest::Client,
    base_url: &str,
    concurrency: usize,
    duration: Duration,
    make_request: F,
) where
    F: Fn(reqwest::Client, String) -> Fut + Send + Sync + 'static + Clone,
    Fut: std::future::Future<Output = (u16, Duration)> + Send,
{
    let metrics = Arc::new(Metrics::new());
    let deadline = Instant::now() + duration;

    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let base_url = base_url.to_string();
        let metrics = Arc::clone(&metrics);
        let make_request = make_request.clone();

        handles.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let (status, elapsed) = make_request(client.clone(), base_url.clone()).await;
                if status == 0 {
                    metrics.record_error();
                } else {
                    metrics.record(status, elapsed);
                }
            }
        }));
    }

    let wall_start = Instant::now();
    for h in handles {
        let _ = h.await;
    }
    let wall_time = wall_start.elapsed();

    metrics.report(label, wall_time);
}

/// Quick non-crypto random usize from timing jitter
fn fastrand_usize() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    nanos.wrapping_mul(2654435761) as usize
}
