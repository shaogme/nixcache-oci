#!/usr/bin/env bash
# test-worker.sh — Real E2E Integration Test against Cloudflare Worker Backend

set -euo pipefail

if [[ -z "${TEST_WORKER_URL:-}" ]]; then
    echo "::error::TEST_WORKER_URL environment variable must be set to run Cloudflare Worker E2E test."
    exit 1
fi

# Strip trailing slash if present
TEST_WORKER_URL="${TEST_WORKER_URL%/}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix Cloudflare Worker E2E Integration Test ==="
echo "Worker URL: $TEST_WORKER_URL"

# Ensure clean state on exit
cleanup() {
    echo ">>> Cleaning up worker test resources..."
    if [[ -f examples/flake/flake.nix.bak ]]; then
        mv -f examples/flake/flake.nix.bak examples/flake/flake.nix
    else
        git checkout -- examples/flake/flake.nix 2>/dev/null || true
    fi
    rm -f test-worker-secret.key test-worker-public.key result-builder-worker
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Generate signing key
echo ">>> Generating signing key pair..."
rm -f test-worker-secret.key test-worker-public.key
nix-store --generate-binary-cache-key test-worker-key-1 test-worker-secret.key test-worker-public.key

# 2. Build builder and proxy binaries
find_binaries() {
    if [[ -n "${BUILDER_BIN:-}" && -x "$BUILDER_BIN" && -n "${PROXY_BIN:-}" && -x "$PROXY_BIN" ]]; then
        echo ">>> Using binaries from environment variables: BUILDER_BIN=$BUILDER_BIN, PROXY_BIN=$PROXY_BIN"
        return 0
    fi

    if [[ -n "${PRECOMPILED_BIN_DIR:-}" && -x "$PRECOMPILED_BIN_DIR/nixcache-builder" && -x "$PRECOMPILED_BIN_DIR/nixcache-proxy" ]]; then
        BUILDER_BIN="$PRECOMPILED_BIN_DIR/nixcache-builder"
        PROXY_BIN="$PRECOMPILED_BIN_DIR/nixcache-proxy"
        echo ">>> Using precompiled binaries from $PRECOMPILED_BIN_DIR"
        return 0
    fi

    if command -v nixcache-builder &>/dev/null && command -v nixcache-proxy &>/dev/null && [[ "${FORCE_BUILD:-false}" != "true" ]]; then
        BUILDER_BIN="$(command -v nixcache-builder)"
        PROXY_BIN="$(command -v nixcache-proxy)"
        echo ">>> Using binaries found in PATH: $BUILDER_BIN, $PROXY_BIN"
        return 0
    fi

    echo ">>> No pre-compiled binaries found. Building nixcache-builder and nixcache-proxy..."
    cargo build -p nixcache-builder -p nixcache-proxy
    BUILDER_BIN="./target/debug/nixcache-builder"
    PROXY_BIN="./target/debug/nixcache-proxy"
}

find_binaries

# 3. Retrieve target registry and repo from Worker status
echo ">>> Fetching Worker status to identify target repo..."
if ! STATUS_JSON=$(curl -fsSL "$TEST_WORKER_URL/_status"); then
    echo "!!! Failed to fetch status from Worker: $TEST_WORKER_URL/_status"
    echo ">>> Attempting verbose fetch for diagnosis:"
    curl -ivL "$TEST_WORKER_URL/_status" || true
    exit 1
fi
echo "Worker status: $STATUS_JSON"

TARGET_REPO=$(echo "$STATUS_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin).get('repo', ''))")
TARGET_REGISTRY=$(echo "$STATUS_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin).get('registry', 'ghcr.io'))")

if [[ -z "$TARGET_REPO" || "$TARGET_REPO" == "null" ]]; then
    echo ">>> Target repo not found in Worker status, detecting from git repository..."
    TARGET_REPO=$(git config --get remote.origin.url | sed -E 's#.*github.com[:/]([^/]+/[^/.]+).*#\1#' 2>/dev/null || echo "shaogme/nixcache-oci")
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

# Modify flake.nix to guarantee a unique hash that has no signatures and is not cached
echo ">>> Modifying examples/flake/flake.nix to generate a unique package hash..."
cp examples/flake/flake.nix examples/flake/flake.nix.bak
sed -i "s/Built at: [^\"]*/Built at: $(date +%s%N)/" examples/flake/flake.nix

if cmp -s examples/flake/flake.nix examples/flake/flake.nix.bak; then
    echo "::error::Failed to mutate examples/flake/flake.nix. Hash will not be unique!"
    exit 1
fi

TEST_STORE_PATH=$(nix build "path:./${NIXCACHE_CONFIG_DIR}#nixcache-test" --no-link --print-out-paths)
echo ">>> Target package store path: $TEST_STORE_PATH"
TEST_HASH=$(basename "$TEST_STORE_PATH" | cut -d'-' -f1)
echo ">>> Target package hash: $TEST_HASH"

# Execute the builder (inject PROXY_BIN directory into PATH so it can spawn nixcache-proxy)
RECEIPT_FILE="$(mktemp --suffix=.json)"
RUST_LOG="${RUST_LOG:-info}" PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN" build --output-receipt "$RECEIPT_FILE"
RUST_LOG="${RUST_LOG:-info}" "$BUILDER_BIN" promote --receipt "$RECEIPT_FILE"
rm -f "$RECEIPT_FILE"

# 5. Deterministic Resource Convergence Verification
# Instead of polling /_status (which has edge KV propagation delays and false convergence),
# we directly verify deterministic convergence on the target .narinfo endpoint.
# With Read-Through SWR self-healing in Worker, this resolves deterministically with zero jitter.
echo ">>> Verifying deterministic convergence on target .narinfo endpoint..."
NARINFO_RESP_HEADERS="$(mktemp)"
NARINFO_CONTENT=""
CONVERGED=false

for i in {1..20}; do
    HTTP_CODE=$(curl -s -o /tmp/narinfo_body.tmp -w "%{http_code}" -D "$NARINFO_RESP_HEADERS" "$TEST_WORKER_URL/${TEST_HASH}.narinfo" || true)
    
    if [[ "$HTTP_CODE" == "200" ]]; then
        NARINFO_CONTENT=$(cat /tmp/narinfo_body.tmp)
        if echo "$NARINFO_CONTENT" | grep -q "StorePath: $TEST_STORE_PATH"; then
            echo ">>> Deterministic convergence achieved at attempt $i!"
            echo ">>> Diagnostic Headers:"
            grep -i "^x-nixcache" "$NARINFO_RESP_HEADERS" || true
            echo ">>> Retrieved narinfo:"
            echo "$NARINFO_CONTENT"
            CONVERGED=true
            break
        fi
    fi
    echo ">>> Waiting for target .narinfo convergence (HTTP $HTTP_CODE), retrying in 2 seconds ($i/20)..."
    sleep 2
done
rm -f "$NARINFO_RESP_HEADERS" /tmp/narinfo_body.tmp

if [[ "$CONVERGED" != "true" ]]; then
    echo "!!! Failed to deterministically converge on target .narinfo after retries."
    exit 1
fi

# 6. Perform substitution test from Worker
echo ">>> Deleting local store path from Nix store (if possible)..."
nix-store --delete "$TEST_STORE_PATH" || true

echo ">>> Realising store path from Cloudflare Worker substituter..."
REALISE_SUCCESS=false
for attempt in 1 2 3; do
    echo ">>> Substitution attempt $attempt/3..."
    if nix-store --realise "$TEST_STORE_PATH" \
      --option substituters "$TEST_WORKER_URL" \
      --option trusted-public-keys "$(cat test-worker-public.key)" \
      --option require-sigs true \
      --option narinfo-cache-negative-ttl 0 \
      --option narinfo-cache-positive-ttl 0 -vvvvv; then
        REALISE_SUCCESS=true
        break
    fi
    echo ">>> Attempt $attempt failed, retrying in 10 seconds..."
    sleep 10
done

if [[ "$REALISE_SUCCESS" != "true" ]]; then
    echo "!!! Failed to realise store path from Worker substituter after 3 attempts."
    exit 1
fi

echo ">>> Verifying the realized package..."
if [[ -x "$TEST_STORE_PATH/bin/nixcache-test" ]]; then
    "$TEST_STORE_PATH/bin/nixcache-test"
else
    echo "!!! Realized package executable not found!"
    exit 1
fi

echo "=== WORKER E2E INTEGRATION TEST PASSED SUCCESSFULLY ==="
