#!/usr/bin/env bash
# test-fault-tolerance.sh — Test nixcache-proxy resilience & fallback on OCI Registry 500/503 errors

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix OCI Cache Fault Tolerance & Upstream Fallback Test ==="

REGISTRY_PORT=5001
UPSTREAM_PORT=5003
PROXY_PORT=37516

UPSTREAM_PID=""
REGISTRY_PID=""
PROXY_PID=""

cleanup() {
    echo ">>> Cleaning up fault tolerance test processes..."
    if [[ -n "${PROXY_PID:-}" ]]; then
        kill -9 "$PROXY_PID" 2>/dev/null || true
    fi
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    if [[ -n "${UPSTREAM_PID:-}" ]]; then
        kill -9 "$UPSTREAM_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/mock-upstream-cache /tmp/mock-oci-registry
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Start Mock Upstream HTTP Server
echo ">>> Setting up mock upstream binary cache on port ${UPSTREAM_PORT}..."
rm -rf /tmp/mock-upstream-cache
mkdir -p /tmp/mock-upstream-cache/nar

cat << 'UPSTREAM_INFO' > /tmp/mock-upstream-cache/faulttest123.narinfo
StorePath: /nix/store/faulttest123-fallback-pkg
URL: nar/faulttest.nar.xz
Compression: xz
FileHash: sha256:1111222233334444555566667777888899990000aaaabbbbccccddddeeeeffff
FileSize: 12
NarHash: sha256:555566667777888899990000aaaabbbbccccddddeeeeffff1111222233334444
NarSize: 12
UPSTREAM_INFO

echo "MOCK_NAR_DATA" > /tmp/mock-upstream-cache/nar/faulttest.nar.xz

python3 -m http.server "${UPSTREAM_PORT}" --directory /tmp/mock-upstream-cache &
UPSTREAM_PID=$!

# 2. Start Mock OCI Registry with Injected 503 Service Unavailable Fault
echo ">>> Launching mock OCI registry with 503 Service Unavailable injection on port ${REGISTRY_PORT}..."
pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
MOCK_FAULT_STATUS=503 python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 3. Build & Launch nixcache-proxy
echo ">>> Building cargo workspace..."
cargo build --bin nixcache-proxy

PROXY_BIN="./target/debug/nixcache-proxy"

export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="fault-test/cache"
export NIXCACHE_LISTEN="127.0.0.1"
export NIXCACHE_PORT="${PROXY_PORT}"
export NIXCACHE_UPSTREAM="http://127.0.0.1:${UPSTREAM_PORT}"
export GITHUB_TOKEN="dummy-token"
unset NIXCACHE_INDEX_DIR
unset CACHE_DIRECTORY

echo ">>> Starting nixcache-proxy on port ${PROXY_PORT}..."
"$PROXY_BIN" &
PROXY_PID=$!

echo ">>> Waiting for proxy to be ready..."
for _ in {1..15}; do
    if curl -fs "http://127.0.0.1:${PROXY_PORT}/nix-cache-info" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "!!! nixcache-proxy failed to start!"
    exit 1
fi

# 4. Verify nix-cache-info
echo ">>> Verifying /nix-cache-info..."
INFO_RESP=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/nix-cache-info")
if ! echo "$INFO_RESP" | grep -q "StoreDir: /nix/store"; then
    echo "!!! /nix-cache-info did not return expected response: $INFO_RESP"
    exit 1
fi
echo ">>> /nix-cache-info verified."

# 5. Verify upstream narinfo fallback when OCI registry is down
echo ">>> Testing narinfo fallback to upstream during OCI registry failure..."
NARINFO_RESP=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/faulttest123.narinfo")
echo "Retrieved narinfo response:"
echo "$NARINFO_RESP"

if ! echo "$NARINFO_RESP" | grep -q "StorePath: /nix/store/faulttest123-fallback-pkg"; then
    echo "!!! Proxy failed to fallback to upstream narinfo!"
    exit 1
fi
echo ">>> Narinfo upstream fallback succeeded."

# 6. Verify upstream NAR blob fallback streaming
echo ">>> Testing NAR blob stream fallback to upstream..."
NAR_RESP=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/nar/faulttest.nar.xz")
if [[ "$NAR_RESP" != "MOCK_NAR_DATA"* ]]; then
    echo "!!! Proxy failed to stream NAR from upstream! Got: $NAR_RESP"
    exit 1
fi
echo ">>> NAR streaming fallback succeeded."

# 7. Verify 404 handling on missing path without crashing
echo ">>> Testing 404 response on unknown store path..."
STATUS_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${PROXY_PORT}/nonexistent123.narinfo")
if [[ "$STATUS_CODE" != "404" ]]; then
    echo "!!! Expected 404, got: $STATUS_CODE"
    exit 1
fi

# 8. Verify /_status reports disconnected remote status during OCI fault
echo ">>> Verifying /_status reports remote disconnection during fault..."
STATUS_RESP=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/_status")
echo "Fault status response: $STATUS_RESP"
if ! echo "$STATUS_RESP" | grep -q '"remote_connected":false'; then
    echo "!!! Expected remote_connected: false in /_status during OCI failure!"
    exit 1
fi

if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "!!! Proxy crashed during fault tests!"
    exit 1
fi

echo "=== FAULT TOLERANCE TEST PASSED SUCCESSFULLY ==="
