#!/usr/bin/env bash
set -euo pipefail

# ── Configuration (all overridable via environment) ───────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SCRIPT_DIR"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cargo-target/edge-analytics}"

DYNAMO_ENDPOINT="${DYNAMO_ENDPOINT:-http://localhost:8000}"
SITEMAP_URL="${SITEMAP_URL:-http://localhost:8888/sitemap.xml}"
SITE_ORIGIN="${SITE_ORIGIN:-http://localhost:9999}"
TABLE_NAME="${TABLE_NAME:-test-views}"
PORT="${PORT:-3001}"
BASE_URL="http://localhost:${PORT}"

CYAN='\033[0;36m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null
    docker compose -f docker-compose.test.yaml down -v 2>/dev/null
}
trap cleanup EXIT

# ── Start infrastructure ──────────────────────────────────────────────
echo -e "${CYAN}Starting DynamoDB Local + sitemap mock...${NC}"
docker compose -f docker-compose.test.yaml up -d

echo -e "${CYAN}Waiting for DynamoDB Local...${NC}"
for i in $(seq 1 30); do
    curl -s "$DYNAMO_ENDPOINT" >/dev/null 2>&1 && break
    sleep 0.5
done

echo -e "${CYAN}Waiting for sitemap mock...${NC}"
for i in $(seq 1 30); do
    curl -sf "$SITEMAP_URL" >/dev/null 2>&1 && break
    sleep 0.5
done

# ── Create DynamoDB table ─────────────────────────────────────────────
echo -e "${CYAN}Creating DynamoDB table...${NC}"
AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test \
aws dynamodb create-table \
    --endpoint-url "$DYNAMO_ENDPOINT" \
    --table-name "$TABLE_NAME" \
    --attribute-definitions \
        AttributeName=path,AttributeType=S \
        AttributeName=dateHour,AttributeType=S \
    --key-schema \
        AttributeName=path,KeyType=HASH \
        AttributeName=dateHour,KeyType=RANGE \
    --billing-mode PAY_PER_REQUEST \
    --region us-east-1 \
    --no-cli-pager 2>/dev/null || echo "(table may already exist)"

# ── Build ─────────────────────────────────────────────────────────────
echo -e "${CYAN}Building release binaries...${NC}"
cargo build --release --features bench 2>&1 | tail -3

# ── Start server ──────────────────────────────────────────────────────
echo -e "${CYAN}Starting edge-analytics server...${NC}"
AWS_ACCESS_KEY_ID=test \
AWS_SECRET_ACCESS_KEY=test \
AWS_REGION=us-east-1 \
AWS_ENDPOINT_URL="$DYNAMO_ENDPOINT" \
TABLE_NAME="$TABLE_NAME" \
SITE_ORIGIN="$SITE_ORIGIN" \
SITEMAP_URL="$SITEMAP_URL" \
PORT="$PORT" \
ENABLE_STATUS=true \
RUST_LOG=warn \
    "$CARGO_TARGET_DIR/release/edge-analytics" &
SERVER_PID=$!

echo -e "${CYAN}Waiting for server (pid $SERVER_PID)...${NC}"
for i in $(seq 1 30); do
    curl -sf "$BASE_URL/status" >/dev/null 2>&1 && break
    sleep 0.5
done

STATUS=$(curl -sf "$BASE_URL/status")
echo -e "${GREEN}Server up: $STATUS${NC}\n"

# ── Run stress test ───────────────────────────────────────────────────
echo -e "${BOLD}${CYAN}Running stress test...${NC}"
SITEMAP_URL="$SITEMAP_URL" SITE_ORIGIN="$SITE_ORIGIN" \
    "$CARGO_TARGET_DIR/release/stress" "$BASE_URL"
