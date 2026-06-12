#!/usr/bin/env bash
# test-worker.sh — Real E2E Integration Test against Cloudflare Worker Backend

set -euo pipefail

if [[ -z "${TEST_WORKER_URL:-}" ]]; then
    echo "TEST_WORKER_URL is not set. Skipping Cloudflare Worker E2E test."
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix Cloudflare Worker E2E Integration Test ==="
echo "Worker URL: $TEST_WORKER_URL"

# Ensure clean state on exit
cleanup() {
    echo ">>> Cleaning up worker test resources..."
    rm -f test-worker-secret.key test-worker-public.key result-builder-worker
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Generate signing key
echo ">>> Generating signing key pair..."
rm -f test-worker-secret.key test-worker-public.key
nix-store --generate-binary-cache-key test-worker-key-1 test-worker-secret.key test-worker-public.key

# 2. Build builder binary
echo ">>> Building nixcache-builder..."
cargo build -p nixcache-builder
BUILDER_BIN="./target/debug/nixcache-builder"

# 3. Retrieve target registry and repo from Worker status
echo ">>> Fetching Worker status to identify target repo..."
STATUS_JSON=$(curl -fs "$TEST_WORKER_URL/_status")
echo "Worker status: $STATUS_JSON"

TARGET_REPO=$(echo "$STATUS_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin).get('repo', ''))")
TARGET_REGISTRY=$(echo "$STATUS_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin).get('registry', 'ghcr.io'))")

if [[ -z "$TARGET_REPO" || "$TARGET_REPO" == "null" ]]; then
    echo "!!! Failed to identify target repo from Worker status."
    exit 1
fi
echo ">>> Target Registry: $TARGET_REGISTRY, Target Repo: $TARGET_REPO"

# 4. Build and push cache to GHCR via Builder
echo ">>> Building and pushing test package to registry..."
export NIXCACHE_REGISTRY="$TARGET_REGISTRY"
export NIXCACHE_REPO="$TARGET_REPO"
export NIXCACHE_SIGNING_KEY_FILE="test-worker-secret.key"
export NIXCACHE_MODE="flake"
export NIXCACHE_CONFIG_DIR="examples/flake"

# Ensure we have GITHUB_TOKEN for registry push
if [[ -z "${GITHUB_TOKEN:-}" ]]; then
    echo "!!! GITHUB_TOKEN environment variable must be set to push to the registry."
    exit 1
fi

TEST_STORE_PATH=$(nix build "./${NIXCACHE_CONFIG_DIR}#nixcache-test" --no-link --print-out-paths)
echo ">>> Target package store path: $TEST_STORE_PATH"
TEST_HASH=$(basename "$TEST_STORE_PATH" | cut -d'-' -f1)
echo ">>> Target package hash: $TEST_HASH"

# Execute the builder to push the package
"$BUILDER_BIN"

# 5. Force Worker to refresh its cache index
echo ">>> Triggering Worker cache index refresh..."
REFRESH_RESP=$(curl -fs -X POST "$TEST_WORKER_URL/_refresh")
echo "Worker refresh response: $REFRESH_RESP"

# 6. Verify Narinfo resolves on Worker
echo ">>> Verifying .narinfo endpoint on Worker..."
NARINFO_CONTENT=$(curl -fs "$TEST_WORKER_URL/${TEST_HASH}.narinfo")
echo ">>> Retrieved narinfo:"
echo "$NARINFO_CONTENT"

if ! echo "$NARINFO_CONTENT" | grep -q "StorePath: $TEST_STORE_PATH"; then
    echo "!!! Retrieved narinfo from Worker does not match target store path!"
    exit 1
fi

# 7. Perform substitution test from Worker
echo ">>> Deleting local store path from Nix store (if possible)..."
nix-store --delete "$TEST_STORE_PATH" || true

echo ">>> Realising store path from Cloudflare Worker substituter..."
nix-store --realise "$TEST_STORE_PATH" \
  --option substituters "$TEST_WORKER_URL" \
  --option trusted-public-keys "$(cat test-worker-public.key)" \
  --option require-sigs true

echo ">>> Verifying the realized package..."
if [[ -x "$TEST_STORE_PATH/bin/nixcache-test" ]]; then
    "$TEST_STORE_PATH/bin/nixcache-test"
else
    echo "!!! Realized package executable not found!"
    exit 1
fi

echo "=== WORKER E2E INTEGRATION TEST PASSED SUCCESSFULLY ==="
