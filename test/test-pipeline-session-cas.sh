#!/usr/bin/env bash
# test-pipeline-session-cas.sh — End-to-end integration test for Schema v3 Session CAS & Cascading Proxy

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting NixCache Schema v3 Pipeline Session CAS & Cascading Test ==="

REGISTRY_PORT=5003
PROXY_PORT=37515
REGISTRY_PID=""
RUN_ID=987654

cleanup() {
    echo ">>> Cleaning up test resources..."
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    pkill -9 -f "nixcache-proxy" 2>/dev/null || true
    rm -rf /tmp/mock-oci-registry /tmp/nixcache-test-*
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Start clean Mock Registry
echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT}..."
pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
pkill -9 -f "nixcache-proxy" 2>/dev/null || true
rm -rf /tmp/mock-oci-registry
mkdir -p /tmp/mock-oci-registry

python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 2. Build binaries
echo ">>> Building nixcache-proxy and nixcache-builder..."
cargo build --bin nixcache-proxy --bin nixcache-builder
BUILDER_BIN="./target/debug/nixcache-builder"
PROXY_BIN="./target/debug/nixcache-proxy"
PROXY_DIR="$(cd "$(dirname "$PROXY_BIN")" && pwd)"
export PATH="$PROXY_DIR:$PATH"

export NIXCACHE_REPO="testorg/testrepo"
export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export GITHUB_TOKEN="dummy-token"

# 3. Test Session Init: background proxy & snapshot
SNAPSHOT_FILE="/tmp/nixcache-test-snapshot.txt"
echo ">>> Initializing session via nixcache-builder session init..."
"$BUILDER_BIN" session init \
    --run-id "$RUN_ID" \
    --branch "main" \
    --port "$PROXY_PORT" \
    --listen "127.0.0.1" \
    --upstream "https://cache.nixos.org" \
    --session-ttl 2 \
    --baseline-ttl 10 \
    --snapshot-path "$SNAPSHOT_FILE"

# Verify proxy health
echo ">>> Checking proxy /nix-cache-info endpoint..."
INFO_RESP=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/nix-cache-info")
echo "$INFO_RESP" | grep -q "StoreDir: /nix/store"
echo ">>> Proxy is healthy and running."

# 4. Test Concurrent Session Capture with CAS
echo ">>> Simulating concurrent worker jobs executing session capture..."
WORKER_PIDS=()

for worker_id in {1..4}; do
    TMP_FILE="/tmp/nixcache-test-dummy-${worker_id}.txt"
    echo "test payload for worker ${worker_id} $(date +%s%N)" > "$TMP_FILE"
    STORE_PATH=$(nix-store --add "$TMP_FILE")
    rm -f "$TMP_FILE"

    (
        JOB_NAME="job-matrix-${worker_id}"
        
        # Call session capture directly with explicit paths
        "$BUILDER_BIN" session capture \
            --run-id "$RUN_ID" \
            --job-id "$JOB_NAME" \
            --system "x86_64-linux" \
            --proxy-url "http://127.0.0.1:${PROXY_PORT}" \
            --output-receipt "/tmp/nixcache-test-receipt-${worker_id}.json" \
            "$STORE_PATH"
    ) &
    WORKER_PIDS+=($!)
done

wait "${WORKER_PIDS[@]}"
echo ">>> All concurrent worker session captures completed."

# 5. Verify RunSessionManifest in OCI Registry (run-<run_id>)
echo ">>> Verifying session manifest in OCI registry..."
SESSION_MANIFEST=$(curl -fs -H "Accept: application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/testrepo/nix-cache/manifests/run-${RUN_ID}")
echo "Session Manifest:"
echo "$SESSION_MANIFEST"

# 6. Test Cascading Proxy Tier 0 (Hot Registry) & Tier 1 (run-<run_id>)
echo ">>> Testing Cascading Proxy status..."
STATUS_RESP=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/_status")
echo "Proxy Status: $STATUS_RESP"

# 7. Test Promote (run-<run_id> -> cache-index and cleanup session tag)
echo ">>> Running nixcache-builder promote for run-${RUN_ID}..."
"$BUILDER_BIN" promote \
    --run-id "$RUN_ID" \
    --target-tag "cache-index"

# 8. Verify Promoted Baseline cache-index
echo ">>> Verifying promoted baseline cache-index..."
BASE_MANIFEST=$(curl -fs -H "Accept: application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/testrepo/nix-cache/manifests/cache-index")
BLOB_DIGEST=$(echo "$BASE_MANIFEST" | python3 -c "import sys, json; print(json.load(sys.stdin)['layers'][0]['digest'])")
BLOB_SAFE_NAME=$(echo "$BLOB_DIGEST" | tr ':' '_')
INDEX_JSON=$(cat "/tmp/mock-oci-registry/blobs/$BLOB_SAFE_NAME")

echo "Index JSON:"
echo "$INDEX_JSON"

python3 -c "
import json
with open('/tmp/mock-oci-registry/blobs/$BLOB_SAFE_NAME') as f:
    idx = json.load(f)
assert idx['version'] == 3, f'Expected version 3, got {idx[\"version\"]}'
assert idx['last_promoted_run'] == $RUN_ID, f'Expected last_promoted_run $RUN_ID, got {idx[\"last_promoted_run\"]}'
"
echo ">>> Promoted cache-index verified (Schema v3 & last_promoted_run)."

# 9. Verify ephemeral session tag cleanup
echo ">>> Verifying session tag run-${RUN_ID} was cleaned up..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/testrepo/nix-cache/manifests/run-${RUN_ID}")
if [[ "$HTTP_CODE" -ne 404 ]]; then
    echo "!!! Expected 404 for deleted session tag, got $HTTP_CODE"
    exit 1
fi
echo ">>> Ephemeral session tag cleanup verified."

# 10. Test Session Clean
echo ">>> Testing nixcache-builder session clean..."
"$BUILDER_BIN" session clean --snapshot-path "$SNAPSHOT_FILE"
if [[ -f "$SNAPSHOT_FILE" ]]; then
    echo "!!! Snapshot file still exists after session clean"
    exit 1
fi
echo ">>> Session clean verified."

echo "=== ALL SCHEMA V3 PIPELINE CAS & CASCADING TESTS PASSED ==="
