#!/usr/bin/env bash
# test-worker.sh — Real E2E Integration Test against Cloudflare Worker Backend

set -euo pipefail

if [[ -z "${TEST_WORKER_URL:-}" ]]; then
    echo "::error::TEST_WORKER_URL environment variable must be set to run Cloudflare Worker E2E test."
    exit 1
fi

# Strip trailing slash if present
TEST_WORKER_URL="${TEST_WORKER_URL%/}"
TEST_WORKER_BASELINE_TAG="${TEST_WORKER_BASELINE_TAG-cache-index-e2e}"
WORKER_SUBSTITUTION_ATTEMPTS="${WORKER_SUBSTITUTION_ATTEMPTS-24}"
WORKER_SUBSTITUTION_DELAY_SECONDS="${WORKER_SUBSTITUTION_DELAY_SECONDS-5}"

validate_positive_integer() {
    local name="$1"
    local value="$2"
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        echo "::error::$name must be a positive integer, got '$value'."
        exit 1
    fi
}

if [[ -z "$TEST_WORKER_BASELINE_TAG" ]]; then
    echo "::error::TEST_WORKER_BASELINE_TAG must not be empty."
    exit 1
fi
if [[ "$TEST_WORKER_BASELINE_TAG" == "cache-index" ]]; then
    echo "::error::TEST_WORKER_BASELINE_TAG must not be the production cache-index tag."
    exit 1
fi
validate_positive_integer "WORKER_SUBSTITUTION_ATTEMPTS" "$WORKER_SUBSTITUTION_ATTEMPTS"
validate_positive_integer "WORKER_SUBSTITUTION_DELAY_SECONDS" "$WORKER_SUBSTITUTION_DELAY_SECONDS"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/nixcache-worker-e2e.XXXXXX")"
SECRET_KEY_FILE="$WORK_DIR/test-worker-secret.key"
PUBLIC_KEY_FILE="$WORK_DIR/test-worker-public.key"
RECEIPT_FILE="$WORK_DIR/receipt.json"
DIAGNOSTIC_HEADERS_FILE="$WORK_DIR/diagnostic.headers"
DIAGNOSTIC_BODY_FILE="$WORK_DIR/diagnostic.body"
FLAKE_FILE="examples/flake/flake.nix"
FLAKE_BACKUP_FILE="$WORK_DIR/flake.nix.bak"
TEST_STORE_PATH=""
WAIT_BUDGET_SECONDS=$(( (WORKER_SUBSTITUTION_ATTEMPTS - 1) * WORKER_SUBSTITUTION_DELAY_SECONDS ))

echo "=== Starting Nix Cloudflare Worker E2E Integration Test ==="
echo "Worker URL: $TEST_WORKER_URL"
echo "Worker baseline tag: $TEST_WORKER_BASELINE_TAG"
echo "Substitution retry budget: $WORKER_SUBSTITUTION_ATTEMPTS attempts, ${WORKER_SUBSTITUTION_DELAY_SECONDS}s delay, ${WAIT_BUDGET_SECONDS}s maximum wait"

# Ensure clean state on exit
cleanup() {
    local exit_code=$?

    echo ">>> Cleaning up worker test resources..."
    if [[ -f "$FLAKE_BACKUP_FILE" ]]; then
        mv -f "$FLAKE_BACKUP_FILE" "$FLAKE_FILE"
    fi
    if [[ -n "$TEST_STORE_PATH" ]]; then
        nix-store --delete "$TEST_STORE_PATH" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK_DIR"
    echo ">>> Cleanup complete."
    return "$exit_code"
}
trap cleanup EXIT

# 1. Generate signing key
echo ">>> Generating signing key pair..."
nix-store --generate-binary-cache-key test-worker-key-1 "$SECRET_KEY_FILE" "$PUBLIC_KEY_FILE"

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
STATUS_BASELINE_TAG=$(echo "$STATUS_JSON" | python3 -c "import sys, json; value=json.load(sys.stdin).get('baseline_tag'); print(value if isinstance(value, str) else '')")

if [[ -z "$STATUS_BASELINE_TAG" ]]; then
    echo "::error::Worker status does not expose a non-empty baseline_tag. Refusing to test an unverified Worker configuration."
    exit 1
fi
if [[ "$STATUS_BASELINE_TAG" != "$TEST_WORKER_BASELINE_TAG" ]]; then
    echo "::error::Worker baseline_tag '$STATUS_BASELINE_TAG' does not match expected '$TEST_WORKER_BASELINE_TAG'."
    exit 1
fi
echo ">>> Worker baseline tag verified: $STATUS_BASELINE_TAG"

if [[ -z "$TARGET_REPO" || "$TARGET_REPO" == "null" ]]; then
    echo ">>> Target repo not found in Worker status, detecting from git repository..."
    TARGET_REPO=$(git config --get remote.origin.url | sed -E 's#.*github.com[:/]([^/]+/[^/.]+).*#\1#' 2>/dev/null || echo "shaogme/nixcache-oci")
fi
echo ">>> Target Registry: $TARGET_REGISTRY, Target Repo: $TARGET_REPO"

# 4. Build and push cache to GHCR via Builder
echo ">>> Building and pushing test package to registry..."
export NIXCACHE_REGISTRY="$TARGET_REGISTRY"
export NIXCACHE_REPO="$TARGET_REPO"
export NIXCACHE_SIGNING_KEY_FILE="$SECRET_KEY_FILE"
export NIXCACHE_BASELINE_TAG="$TEST_WORKER_BASELINE_TAG"
export NIXCACHE_MODE="flake"
export NIXCACHE_CONFIG_DIR="examples/flake"

# Ensure we have GITHUB_TOKEN for registry push
if [[ -z "${GITHUB_TOKEN:-}" ]]; then
    echo "!!! GITHUB_TOKEN environment variable must be set to push to the registry."
    exit 1
fi

# Modify flake.nix to guarantee a unique hash that has no signatures and is not cached
echo ">>> Modifying examples/flake/flake.nix to generate a unique package hash..."
cp "$FLAKE_FILE" "$FLAKE_BACKUP_FILE"
sed -i "s/Built at: [^\"]*/Built at: $(date +%s%N)/" "$FLAKE_FILE"

if cmp -s "$FLAKE_FILE" "$FLAKE_BACKUP_FILE"; then
    echo "::error::Failed to mutate examples/flake/flake.nix. Hash will not be unique!"
    exit 1
fi

TEST_STORE_PATH=$(nix build "path:./${NIXCACHE_CONFIG_DIR}#nixcache-test" --no-link --print-out-paths)
echo ">>> Target package store path: $TEST_STORE_PATH"
TEST_HASH=$(basename "$TEST_STORE_PATH" | cut -d'-' -f1)
echo ">>> Target package hash: $TEST_HASH"

# Execute the builder (inject PROXY_BIN directory into PATH so it can spawn nixcache-proxy)
RUST_LOG="${RUST_LOG:-info}" PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN" build --output-receipt "$RECEIPT_FILE"
RUST_LOG="${RUST_LOG:-info}" "$BUILDER_BIN" promote \
    --receipt "$RECEIPT_FILE" \
    --target-tag "$TEST_WORKER_BASELINE_TAG"

# 5. Perform substitution test from Worker
echo ">>> Deleting local store path from Nix store (if possible)..."
nix-store --delete "$TEST_STORE_PATH" || true

echo ">>> Realising store path from Cloudflare Worker substituter..."
echo ">>> narinfo-cache-negative-ttl=0 and narinfo-cache-positive-ttl=0 disable only local Nix narinfo caching; they do not provide remote strong consistency."
REALISE_SUCCESS=false
for ((attempt = 1; attempt <= WORKER_SUBSTITUTION_ATTEMPTS; attempt++)); do
    echo ">>> Substitution attempt $attempt/$WORKER_SUBSTITUTION_ATTEMPTS..."
    if nix-store --realise "$TEST_STORE_PATH" \
      --option substituters "$TEST_WORKER_URL" \
      --option trusted-public-keys "$(cat "$PUBLIC_KEY_FILE")" \
      --option require-sigs true \
      --option narinfo-cache-negative-ttl 0 \
      --option narinfo-cache-positive-ttl 0 \
      -vvvvv; then
        REALISE_SUCCESS=true
        break
    fi
    if (( attempt < WORKER_SUBSTITUTION_ATTEMPTS )); then
        echo ">>> Attempt $attempt failed, retrying in ${WORKER_SUBSTITUTION_DELAY_SECONDS} seconds..."
        sleep "$WORKER_SUBSTITUTION_DELAY_SECONDS"
    fi
done

if [[ "$REALISE_SUCCESS" != "true" ]]; then
    echo "!!! Failed to realise store path from Worker substituter after $WORKER_SUBSTITUTION_ATTEMPTS attempts."
    echo ">>> Running one failure-only diagnostic observation for ${TEST_HASH}.narinfo..."
    DIAGNOSTIC_HTTP_CODE="000"
    if ! DIAGNOSTIC_HTTP_CODE=$(curl -sS \
        -o "$DIAGNOSTIC_BODY_FILE" \
        -D "$DIAGNOSTIC_HEADERS_FILE" \
        -w "%{http_code}" \
        "$TEST_WORKER_URL/${TEST_HASH}.narinfo"); then
        echo ">>> Diagnostic curl failed before receiving an HTTP response."
    fi
    echo ">>> Diagnostic observation (not a success gate): HTTP $DIAGNOSTIC_HTTP_CODE"
    for header in cf-ray x-nixcache-digest x-nixcache-self-healed x-nixcache-shard; do
        header_value=$(awk -F': ' -v name="$header" \
            'tolower($1) == tolower(name) { gsub("\\r", "", $2); print $2; exit }' \
            "$DIAGNOSTIC_HEADERS_FILE" 2>/dev/null || true)
        echo "    $header: ${header_value:-<missing>}"
    done
    if [[ -f "$DIAGNOSTIC_BODY_FILE" ]] && grep -Fq "StorePath: $TEST_STORE_PATH" "$DIAGNOSTIC_BODY_FILE"; then
        echo ">>> Diagnostic response contains target StorePath: yes"
    else
        echo ">>> Diagnostic response contains target StorePath: no"
    fi
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
