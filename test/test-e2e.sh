#!/usr/bin/env bash
# test-e2e.sh — Full E2E Integration Test using a local OCI Registry

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix OCI Cache E2E Integration Test ==="

# 1. Start a local OCI registry container or mock registry
REGISTRY_CONTAINER="nixcache-test-registry"
REGISTRY_PORT=5001
REGISTRY_PID=""

if command -v docker &>/dev/null && docker ps &>/dev/null; then
    if docker ps -a --format '{{.Names}}' | grep -q "^${REGISTRY_CONTAINER}$"; then
        echo ">>> Stopping existing registry container..."
        docker rm -f "$REGISTRY_CONTAINER" >/dev/null
    fi

    echo ">>> Launching local OCI registry via Docker..."
    docker run -d -p "${REGISTRY_PORT}:5000" --name "$REGISTRY_CONTAINER" registry:2
else
    echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT}..."
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    lsof -ti:"${REGISTRY_PORT}" | xargs -r kill -9 2>/dev/null || true
    python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
    REGISTRY_PID=$!
    for _ in {1..20}; do
        if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
            break
        fi
        sleep 0.5
    done

fi

# Ensure registry container is cleaned up on exit
cleanup() {
    echo ">>> Cleaning up resources..."
    git checkout -- examples/flake/flake.nix examples/legacy/default.nix 2>/dev/null || true
    if [[ -n "${PROXY_PID:-}" ]]; then
        kill -9 "$PROXY_PID" 2>/dev/null || true
    fi
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    if command -v docker &>/dev/null && docker ps &>/dev/null; then
        docker rm -f "$REGISTRY_CONTAINER" >/dev/null 2>&1 || true
    fi
    rm -rf /tmp/mock-oci-registry
    rm -f test-secret.key test-public.key result-builder result-proxy result-builder-bin result-proxy-bin
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 2. Generate signing key
echo ">>> Generating signing key pair..."
rm -f test-secret.key test-public.key
nix-store --generate-binary-cache-key test-key-1 test-secret.key test-public.key

# 3. Build builder and proxy binaries
BUILD_MODE="${1:-cargo}"
TEST_MODE="${2:-flake}"
echo ">>> Building in mode: $BUILD_MODE, Testing in mode: $TEST_MODE"

if [[ "$BUILD_MODE" == "cargo" ]]; then
    echo ">>> Building cargo workspace..."
    cargo build --workspace
    BUILDER_BIN="./target/debug/nixcache-builder"
    PROXY_BIN="./target/debug/nixcache-proxy"
elif [[ "$BUILD_MODE" == "nix-source" ]]; then
    echo ">>> Building packages from Nix source..."
    nix-build default.nix -A cache-builder --out-link result-builder
    nix-build default.nix -A cache-proxy --out-link result-proxy
    BUILDER_BIN="./result-builder/bin/nixcache-builder"
    PROXY_BIN="./result-proxy/bin/nixcache-proxy"
elif [[ "$BUILD_MODE" == "nix-bin" ]]; then
    echo ">>> Fetching packages from Nix pre-built binaries..."
    nix-build default.nix -A cache-builder-bin --out-link result-builder-bin
    nix-build default.nix -A cache-proxy-bin --out-link result-proxy-bin
    BUILDER_BIN="./result-builder-bin/bin/nixcache-builder"
    PROXY_BIN="./result-proxy-bin/bin/nixcache-proxy"
else
    echo "!!! Unknown BUILD_MODE: $BUILD_MODE"
    exit 1
fi

# 4. Run builder to build and push cache to local OCI registry
echo ">>> Running nixcache-builder..."
export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="test/cache"
export NIXCACHE_SIGNING_KEY_FILE="test-secret.key"
export GITHUB_TOKEN="dummy-token"

# 加载环境变量
# shellcheck disable=SC1091
source "$(dirname "$0")/../scripts/load-env.sh" "$TEST_MODE"

if [[ "${NIXCACHE_MODE:-flake}" == "flake" ]]; then
    sed -i "s/Built at: .*/Built at: $(date +%s%N)\"/" examples/flake/flake.nix
    TEST_STORE_PATH=$(nix build "./${NIXCACHE_CONFIG_DIR}#nixcache-test" --no-link --print-out-paths)
elif [[ "${NIXCACHE_MODE:-}" == "non-flake" ]]; then
    sed -i "s/Built at: .*/Built at: $(date +%s%N)\"/" examples/legacy/default.nix
    TEST_STORE_PATH=$(nix build --file "${NIXCACHE_FILE}" "${NIXCACHE_ATTRIBUTES}" --no-link --print-out-paths)
else
    echo "!!! Unknown NIXCACHE_MODE: $NIXCACHE_MODE"
    exit 1
fi

echo ">>> Target package store path: $TEST_STORE_PATH"
TEST_HASH=$(basename "$TEST_STORE_PATH" | cut -d'-' -f1)
echo ">>> Target package hash: $TEST_HASH"

# Execute the builder (inject PROXY_BIN directory into PATH so it can spawn nixcache-proxy)
PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN" all-in-one

# 5. Start proxy pointing to the local registry
echo ">>> Starting nixcache-proxy..."
export NIXCACHE_LISTEN="127.0.0.1"
export NIXCACHE_PORT="37515"
export NIXCACHE_UPSTREAM=""
# Disable cache dir environment if set to avoid using home dir cache
unset NIXCACHE_INDEX_DIR
unset CACHE_DIRECTORY

"$PROXY_BIN" &
PROXY_PID=$!

echo ">>> Waiting for proxy to become ready..."
for _ in {1..10}; do
    if curl -fs http://127.0.0.1:37515/nix-cache-info >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "!!! Proxy failed to start"
    exit 1
fi

# 6. Verify endpoints
echo ">>> Verifying public key endpoint..."
FETCHED_PUBKEY=$(curl -fs http://127.0.0.1:37515/public-key)
EXPECTED_PUBKEY=$(cat test-public.key)
if [[ "$FETCHED_PUBKEY" != "$EXPECTED_PUBKEY"* ]]; then
    echo "!!! Public key mismatch. Expected: $EXPECTED_PUBKEY, Got: $FETCHED_PUBKEY"
    exit 1
fi
echo ">>> Public key verified successfully."

echo ">>> Verifying /_status endpoint..."
STATUS_RESP=$(curl -fs "http://127.0.0.1:37515/_status")
echo "Status response: $STATUS_RESP"
if ! echo "$STATUS_RESP" | grep -q '"remote_connected":true'; then
    echo "!!! Expected remote_connected: true in /_status!"
    exit 1
fi

echo ">>> Verifying .narinfo endpoint..."
# Force index refresh first to fetch the newly uploaded cache-index
curl -fs -X POST http://127.0.0.1:37515/_refresh || true

NARINFO_CONTENT=$(curl -fs "http://127.0.0.1:37515/${TEST_HASH}.narinfo")
echo ">>> Retrieved narinfo:"
echo "$NARINFO_CONTENT"

if ! echo "$NARINFO_CONTENT" | grep -q "StorePath: $TEST_STORE_PATH"; then
    echo "!!! Retrieved narinfo does not match target store path!"
    exit 1
fi

# 7. Perform substitution test
echo ">>> Deleting local store path from Nix store (if possible)..."
nix-store --delete "$TEST_STORE_PATH" || true

echo ">>> Realising store path from local proxy substituter..."
nix-store --realise "$TEST_STORE_PATH" \
  --option substituters "http://127.0.0.1:37515" \
  --option trusted-public-keys "$(cat test-public.key)" \
  --option require-sigs true

echo ">>> Verifying the realized package..."
if [[ -x "$TEST_STORE_PATH/bin/nixcache-test" ]]; then
    "$TEST_STORE_PATH/bin/nixcache-test"
else
    echo "!!! Realized package executable not found!"
    exit 1
fi
echo "=== E2E INTEGRATION TEST PASSED SUCCESSFULLY ==="
