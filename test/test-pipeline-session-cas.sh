#!/usr/bin/env bash
# test-pipeline-session-cas.sh — End-to-end integration test for Schema v4 Session CAS & Cascading Proxy

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting NixCache Schema v4 Pipeline Session CAS & Cascading Test ==="

TMP_DIR=$(mktemp -d /tmp/nixcache-pipeline-test-XXXXXX)
export GITHUB_ENV="$TMP_DIR/github_env"
export GITHUB_OUTPUT="$TMP_DIR/github_output"
export GITHUB_PATH="$TMP_DIR/github_path"
touch "$GITHUB_ENV" "$GITHUB_OUTPUT" "$GITHUB_PATH"

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
    rm -rf /tmp/mock-oci-registry /tmp/nixcache-test-* "$TMP_DIR"
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

    echo ">>> No pre-compiled binaries found. Building nixcache-proxy and nixcache-builder..."
    cargo build --bin nixcache-proxy --bin nixcache-builder
    BUILDER_BIN="./target/debug/nixcache-builder"
    PROXY_BIN="./target/debug/nixcache-proxy"
}

find_binaries
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

# 5. Verify ArchRunSessionManifest in OCI Registry (run-<run_id>-x86_64-linux)
echo ">>> Verifying session manifest in OCI registry..."
SESSION_MANIFEST=$(curl -fs -H "Accept: application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/testrepo/nix-cache/manifests/run-${RUN_ID}-x86_64-linux")
echo "Session Manifest:"
echo "$SESSION_MANIFEST"

python3 -c "
import json, subprocess
manifest = json.loads('''$SESSION_MANIFEST''')
layer_digest = manifest['layers'][0]['digest']
layer_safe = layer_digest.replace(':', '_')
blob_path = f'/tmp/mock-oci-registry/blobs/{layer_safe}'
decompressed = subprocess.check_output(['zstd', '-dc', blob_path])
session_data = json.loads(decompressed)
assert session_data['version'] == 4, f'Expected version 4, got {session_data[\"version\"]}'
assert session_data['run_id'] == $RUN_ID, f'Expected run_id $RUN_ID, got {session_data[\"run_id\"]}'
assert len(session_data['entries']) == 4, f'Expected 4 entries from 4 workers, got {len(session_data[\"entries\"])}'
print('>>> Session manifest verified: Schema v4, 4 entries merged via CAS.')
"

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
BASE_INDEX=$(curl -fs -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/testrepo/nix-cache/manifests/cache-index")
echo "Base Index Manifest:"
echo "$BASE_INDEX"

python3 -c "
import json, subprocess
manifest_index = json.loads('''$BASE_INDEX''')
sub_manifest_digest = manifest_index['manifests'][0]['digest']
sub_safe = sub_manifest_digest.replace(':', '_')

with open(f'/tmp/mock-oci-registry/manifests/{sub_safe}', 'rb') as f:
    sub_manifest = json.load(f)

layer_digest = sub_manifest['layers'][0]['digest']
layer_safe = layer_digest.replace(':', '_')
blob_path = f'/tmp/mock-oci-registry/blobs/{layer_safe}'

decompressed = subprocess.check_output(['zstd', '-dc', blob_path])
idx = json.loads(decompressed)
assert idx['version'] == 4, f'Expected version 4, got {idx[\"version\"]}'
assert idx['last_promoted_run'] == $RUN_ID, f'Expected last_promoted_run $RUN_ID, got {idx[\"last_promoted_run\"]}'
assert len(idx['entries']) == 4, f'Expected 4 promoted entries, got {len(idx[\"entries\"])}'
print('>>> Promoted cache-index verified (Schema v4, last_promoted_run & 4 entries).')
"

# 9. Verify ephemeral session tag cleanup
echo ">>> Verifying session tag run-${RUN_ID}-x86_64-linux was cleaned up..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/testrepo/nix-cache/manifests/run-${RUN_ID}-x86_64-linux")
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

echo "=== ALL SCHEMA V4 PIPELINE CAS & CASCADING TESTS PASSED ==="
