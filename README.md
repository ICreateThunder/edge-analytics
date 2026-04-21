# edge-analytics

Privacy-respecting page view counter for static sites. No cookies, no IP addresses, no PII stored.

## Architecture

A Rust binary (axum) running on an EC2 instance behind Caddy (auto TLS), writing anonymous counters to DynamoDB.

```
Browser (sendBeacon) → Caddy (:443) → edge-analytics (:3001) → DynamoDB
```

### Security

- **Path whitelist from sitemap** — only paths present in your sitemap are accepted. Refreshed hourly. Prevents arbitrary input reaching the database (GDPR, storage abuse, injection).
- **Token-bucket rate limiter** — 60 requests per path per minute, in-process. Excess requests are rejected before touching DynamoDB.
- **DynamoDB conditional writes** — hourly counter cap prevents inflation even if rate limiter is bypassed.
- **CORS locked to origin** — only your configured domain can send beacons.
- **No PII** — the binary never reads, logs, or stores IP addresses, user agents, or any identifying information. Only `{path, dateHour, count}` reaches the database.
- **Fixed cost** — runs on a small EC2 instance (t4g.nano). Cost does not scale with traffic, making it immune to billing attacks.

### DynamoDB schema

| Key | Type | Example |
|-----|------|---------|
| `path` (PK) | String | `/`, `/projects/my-project` |
| `dateHour` (SK) | String | `2026-04-21T14` |
| `views` | Number | `47` |

## Configuration

All configuration is via environment variables:

| Variable | Required | Description |
|----------|----------|-------------|
| `TABLE_NAME` | Yes | DynamoDB table name |
| `SITE_ORIGIN` | Yes | Your site origin (e.g. `https://example.com`) |
| `SITEMAP_URL` | Yes | Full URL to your sitemap XML |
| `AWS_REGION` | Yes | AWS region for DynamoDB |
| `PORT` | No | Listen port (default: `3001`) |
| `ENABLE_STATUS` | No | Expose `/status` endpoint (default: `false`) |

AWS credentials are picked up from the EC2 instance IAM role via IMDS — no static keys needed.

## Deployment

1. Create a DynamoDB table (on-demand billing) with `path` (String) as partition key and `dateHour` (String) as sort key.

2. Create an IAM role for your EC2 instance with `dynamodb:UpdateItem` and `dynamodb:Query` on the table.

3. Copy the example files and configure:
```bash
cp .env.example .env
cp Caddyfile.example Caddyfile
cp docker-compose.example.yaml docker-compose.yaml
# Edit .env, Caddyfile with your domain
```

4. Run:
```bash
docker compose up -d --build
```

Caddy automatically provisions TLS certificates via Let's Encrypt.

## Client integration

Add to your site's `<head>`:

```html
<script>
  navigator.sendBeacon("https://analytics.yourdomain.com/views", JSON.stringify({ path: location.pathname }));
</script>
```

## Querying

```bash
# Views for a specific path today
aws dynamodb query --table-name your-table \
  --key-condition-expression "path = :p AND begins_with(dateHour, :d)" \
  --expression-attribute-values '{":p":{"S":"/"},":d":{"S":"2026-04-21"}}'

# All data
aws dynamodb scan --table-name your-table --projection-expression "path, dateHour, views"
```

## License

MIT
