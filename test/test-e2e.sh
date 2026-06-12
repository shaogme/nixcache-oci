#!/usr/bin/env bash
# test-e2e.sh — Full E2E Integration Test using a local OCI Registry

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix OCI Cache E2E Integration Test ==="

# 1. Start a local OCI registry container
REGISTRY_CONTAINER="nixcache-test-registry"
REGISTRY_PORT=5001

if docker ps -a --format '{{.Names}}' | grep -q "^${REGISTRY_CONTAINER}$"; then
    echo ">>> Stopping existing registry container..."
    docker rm -f "$REGISTRY_CONTAINER" >/dev/null
fi

echo ">>> Launching local OCI registry..."
docker run -d -p "${REGISTRY_PORT}:5000" --name "$REGISTRY_CONTAINER" registry:2

# Ensure registry container is cleaned up on exit
cleanup() {
    echo ">>> Cleaning up resources..."
    if [[ -n "${PROXY_PID:-}" ]]; then
        kill "$PROXY_PID" 2>/dev/null || true
    fi
    docker rm -f "$REGISTRY_CONTAINER" >/dev/null 2>&1 || true
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
echo ">>> Building in mode: $BUILD_MODE"

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
export NIXCACHE_CONFIG_DIR="config"

# Get the target store path of our test package
TEST_STORE_PATH=$(nix build ./config#nixcache-test --no-link --print-out-paths)
echo ">>> Target package store path: $TEST_STORE_PATH"
TEST_HASH=$(basename "$TEST_STORE_PATH" | cut -d'-' -f1)
echo ">>> Target package hash: $TEST_HASH"

# Execute the builder (inject PROXY_BIN directory into PATH so it can spawn nixcache-proxy)
PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN"

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
for i in {1..10}; do
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
