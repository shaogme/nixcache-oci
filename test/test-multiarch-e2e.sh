#!/usr/bin/env bash
# test-multiarch-e2e.sh — Multi-Arch Scatter-Gather E2E Integration Test using a local OCI Registry

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix Multi-Arch Scatter-Gather E2E Integration Test ==="

# 1. Start a local OCI registry container or mock registry
REGISTRY_CONTAINER="nixcache-multiarch-registry"
REGISTRY_PORT=5002
REGISTRY_PID=""

if command -v docker &>/dev/null && docker ps &>/dev/null; then
    if docker ps -a --format '{{.Names}}' | grep -q "^${REGISTRY_CONTAINER}$"; then
        echo ">>> Stopping existing registry container..."
        docker rm -f "$REGISTRY_CONTAINER" >/dev/null
    fi

    echo ">>> Launching local OCI registry on port ${REGISTRY_PORT} via Docker..."
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

# Cleanup on exit
RECEIPTS_DIR="$(mktemp -d)"
cleanup() {
    echo ">>> Cleaning up multiarch test resources..."
    git checkout -- examples/flake/flake.nix 2>/dev/null || true
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
    rm -rf "$RECEIPTS_DIR" /tmp/mock-oci-registry
    rm -f test-multi-secret.key test-multi-public.key
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 2. Generate signing key
echo ">>> Generating signing key pair..."
rm -f test-multi-secret.key test-multi-public.key
nix-store --generate-binary-cache-key test-multi-key-1 test-multi-secret.key test-multi-public.key

# 3. Build builder and proxy binaries
echo ">>> Building cargo workspace..."
cargo build --workspace
BUILDER_BIN="./target/debug/nixcache-builder"
PROXY_BIN="./target/debug/nixcache-proxy"

export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="test/multiarch"
export NIXCACHE_SIGNING_KEY_FILE="test-multi-secret.key"
export GITHUB_TOKEN="dummy-token"
PROXY_DIR="$(cd "$(dirname "$PROXY_BIN")" && pwd)"
export PATH="$PROXY_DIR:$PATH"


# =========================================================================
# Phase 1: Scatter (Parallel Worker builds producing Receipts)
# =========================================================================

echo ">>> Modifying examples/flake/flake.nix with unique timestamp..."
sed -i "s/Built at: .*/Built at: $(date +%s%N)\"/" examples/flake/flake.nix

echo ">>> [Phase 1] Running Worker 1 (Flake mode)..."
RECEIPT_1="$RECEIPTS_DIR/receipt-worker-1.json"
"$BUILDER_BIN" build \
    --mode flake \
    --flake-path "examples/flake" \
    --repo "$NIXCACHE_REPO" \
    --registry "$NIXCACHE_REGISTRY" \
    --signing-key-file "$NIXCACHE_SIGNING_KEY_FILE" \
    --output-receipt "$RECEIPT_1" \
    --fail-fast

if [[ ! -f "$RECEIPT_1" ]]; then
    echo "!!! Worker 1 failed to generate receipt: $RECEIPT_1"
    exit 1
fi
echo ">>> Worker 1 receipt generated successfully:"
cat "$RECEIPT_1"
echo ""

echo ">>> [Phase 1] Running Worker 2 (Non-Flake mode / custom package target)..."
CUSTOM_PKG_FILE="$(mktemp --suffix=.nix)"
cat << EOF > "$CUSTOM_PKG_FILE"
let
  sources = import $PROJECT_DIR/npins;
  pkgs = import sources.nixpkgs {};
in
pkgs.writeShellScriptBin "nixcache-custom-multiarch" ''
  echo "Hello from multiarch worker 2!"
  echo "Timestamp: $(date +%s%N)"
''
EOF

RECEIPT_2="$RECEIPTS_DIR/receipt-worker-2.json"
"$BUILDER_BIN" build \
    --system "aarch64-linux" \
    --mode non-flake \
    --file "$CUSTOM_PKG_FILE" \
    --repo "$NIXCACHE_REPO" \
    --registry "$NIXCACHE_REGISTRY" \
    --signing-key-file "$NIXCACHE_SIGNING_KEY_FILE" \
    --output-receipt "$RECEIPT_2" \
    --fail-fast

rm -f "$CUSTOM_PKG_FILE"

if [[ ! -f "$RECEIPT_2" ]]; then
    echo "!!! Worker 2 failed to generate receipt: $RECEIPT_2"
    exit 1
fi
echo ">>> Worker 2 receipt generated successfully:"
cat "$RECEIPT_2"
echo ""

# =========================================================================
# Phase 2: Gather (Merge Coordinator aggregating Receipts & publishing Index)
# =========================================================================

echo ">>> [Phase 2] Running Merge Coordinator..."
"$BUILDER_BIN" merge \
    --receipts-dir "$RECEIPTS_DIR" \
    --repo "$NIXCACHE_REPO" \
    --registry "$NIXCACHE_REGISTRY"

echo ">>> Fetching published cache-index from local registry to verify Schema v3..."
INDEX_MANIFEST=$(curl -fsSL -H "Accept: application/vnd.oci.image.manifest.v1+json" "http://${NIXCACHE_REGISTRY}/v2/${NIXCACHE_REPO}/nix-cache/manifests/cache-index")
INDEX_DIGEST=$(echo "$INDEX_MANIFEST" | python3 -c "import sys, json; print(json.load(sys.stdin)['layers'][0]['digest'])")
INDEX_JSON=$(curl -fsSL "http://${NIXCACHE_REGISTRY}/v2/${NIXCACHE_REPO}/nix-cache/blobs/${INDEX_DIGEST}")

echo ">>> Published Cache Index v3 Content:"
echo "$INDEX_JSON" | python3 -m json.tool

# Verify Schema v3 and multi-arch entries
echo "$INDEX_JSON" | python3 -c "
import sys, json
data = json.load(sys.stdin)
assert data['version'] == 3, f'Expected version 3, got {data[\"version\"]}'
assert len(data['entries']) >= 2, f'Expected at least 2 entries, got {len(data[\"entries\"])}'
assert 'x86_64-linux' in data['gc_roots'], f'Missing x86_64-linux in gc_roots: {data[\"gc_roots\"]}'
assert 'aarch64-linux' in data['gc_roots'], f'Missing aarch64-linux in gc_roots: {data[\"gc_roots\"]}'
print('>>> Schema v3 and Multi-Arch verification SUCCESS!')
"

# =========================================================================
# Phase 3: Client Verification via nixcache-proxy
# =========================================================================

echo ">>> [Phase 3] Starting nixcache-proxy..."
export NIXCACHE_LISTEN="127.0.0.1"
export NIXCACHE_PORT="37516"
export NIXCACHE_UPSTREAM=""
unset NIXCACHE_INDEX_DIR
unset CACHE_DIRECTORY

"$PROXY_BIN" --port 37516 &
PROXY_PID=$!

echo ">>> Waiting for proxy to become ready on port 37516..."
for _ in {1..10}; do
    if curl -fs http://127.0.0.1:37516/nix-cache-info >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "!!! Proxy failed to start"
    exit 1

fi

echo ">>> Testing public key endpoint on proxy..."
FETCHED_PUBKEY=$(curl -fs http://127.0.0.1:37516/public-key)
EXPECTED_PUBKEY=$(cat test-multi-public.key)
if [[ "$FETCHED_PUBKEY" != "$EXPECTED_PUBKEY"* ]]; then
    echo "!!! Public key mismatch: Expected $EXPECTED_PUBKEY, got $FETCHED_PUBKEY"
    exit 1
fi
echo ">>> Public key verified on proxy."

# Fetch each entry hash and verify narinfo endpoint
TEST_HASHES=$(echo "$INDEX_JSON" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for h in data['entries'].keys():
    print(h)
")

for HASH in $TEST_HASHES; do
    echo ">>> Testing .narinfo query for hash: $HASH"
    NARINFO=$(curl -fs "http://127.0.0.1:37516/${HASH}.narinfo")
    echo "$NARINFO"
    if ! echo "$NARINFO" | grep -q "StorePath:"; then
        echo "!!! Invalid narinfo for hash $HASH"
        exit 1
    fi
done

# Perform full Nix substitution on one of the store paths
SAMPLE_STORE_PATH=$(echo "$INDEX_JSON" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for entry in data['entries'].values():
    for line in entry['narinfo'].splitlines():
        if line.startswith('StorePath: '):
            print(line.split('StorePath: ')[1].strip())
            sys.exit(0)
")

echo ">>> Testing substitution for: $SAMPLE_STORE_PATH"
nix-store --delete "$SAMPLE_STORE_PATH" 2>/dev/null || true

nix-store --realise "$SAMPLE_STORE_PATH" \
  --option substituters "http://127.0.0.1:37516" \
  --option trusted-public-keys "$(cat test-multi-public.key)" \
  --option require-sigs true

if [[ -e "$SAMPLE_STORE_PATH" ]]; then
    echo ">>> Realised store path exists: $SAMPLE_STORE_PATH"
else
    echo "!!! Failed to realise store path $SAMPLE_STORE_PATH"
    exit 1
fi

# =========================================================================
# Phase 4: Garbage Collection (Multi-Arch Aware)
# =========================================================================

echo ">>> [Phase 4] Testing cross-architecture GC dry-run..."
"$BUILDER_BIN" gc \
    --repo "$NIXCACHE_REPO" \
    --registry "$NIXCACHE_REGISTRY" \
    --retention-days 30 \
    --dry-run

echo "=== MULTI-ARCH SCATTER-GATHER E2E TEST PASSED SUCCESSFULLY ==="
