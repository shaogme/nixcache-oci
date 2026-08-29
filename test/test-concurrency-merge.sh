#!/usr/bin/env bash
# test-concurrency-merge.sh — Test concurrency merge, idempotency & multi-arch GC aggregation

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix OCI Cache Concurrency Merge & GC Roots Test ==="

REGISTRY_PORT=5001
REGISTRY_PID=""
RECEIPTS_DIR="/tmp/test-concurrency-receipts"

cleanup() {
    echo ">>> Cleaning up concurrency merge test resources..."
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf "$RECEIPTS_DIR" /tmp/mock-oci-registry
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Start clean Mock Registry
echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT}..."
pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
rm -rf /tmp/mock-oci-registry "$RECEIPTS_DIR"
mkdir -p "$RECEIPTS_DIR"

python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 2. Build nixcache-builder binary
echo ">>> Building nixcache-builder..."
cargo build --bin nixcache-builder
BUILDER_BIN="./target/debug/nixcache-builder"

# 3. Simulate 12 concurrent workers generating receipts with overlapping and distinct packages
echo ">>> Simulating 12 concurrent workers generating build receipts..."

SYSTEMS=("x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin")
WORKER_PIDS=()

for worker_id in {1..12}; do
    SYS_INDEX=$(( (worker_id - 1) % 4 ))
    SYS="${SYSTEMS[$SYS_INDEX]}"
    RECEIPT_FILE="$RECEIPTS_DIR/receipt-worker-${worker_id}-${SYS}.json"
    
    python3 - "$worker_id" "$SYS" "$RECEIPT_FILE" << 'PYEOF' &
import json, sys

worker_id = int(sys.argv[1])
sys_name = sys.argv[2]
receipt_file = sys.argv[3]

entries = {
    'hash-shared-libc': {
        'name': 'glibc-common',
        'system': sys_name,
        'narinfo': 'StorePath: /nix/store/hash-shared-libc-glibc\n',
        'nar_digest': 'sha256:digest-libc',
        'nar_size': 5000,
        'added': '2026-08-28T00:00:00Z'
    },
    f'hash-{sys_name}-base': {
        'name': f'{sys_name}-base',
        'system': sys_name,
        'narinfo': f'StorePath: /nix/store/hash-{sys_name}-base\n',
        'nar_digest': f'sha256:digest-{sys_name}-base',
        'nar_size': 10000,
        'added': '2026-08-28T00:00:00Z'
    },
    f'hash-pkg-worker-{worker_id}': {
        'name': f'pkg-worker-{worker_id}',
        'system': sys_name,
        'narinfo': f'StorePath: /nix/store/hash-pkg-worker-{worker_id}\n',
        'nar_digest': f'sha256:digest-worker-{worker_id}',
        'nar_size': 2000,
        'added': '2026-08-28T00:00:00Z'
    }
}

active_roots = [
    'hash-shared-libc',
    f'hash-{sys_name}-base',
    f'hash-pkg-worker-{worker_id}'
]

receipt = {
    'version': 3,
    'system': sys_name,
    'repo': 'concurrency-test/cache',
    'timestamp': '2026-08-28T00:00:00Z',
    'public_key': 'test-concurrency-key:AAAA=',
    'new_entries': entries,
    'active_gc_roots': active_roots,
    'stats': {
        'discovered_outputs': 3,
        'built_paths': 3,
        'uploaded_blobs': 3,
        'total_bytes_uploaded': 17000
    }
}

with open(receipt_file, 'w') as f:
    json.dump(receipt, f, indent=2)
PYEOF
    WORKER_PIDS+=($!)
done

wait "${WORKER_PIDS[@]}"
echo ">>> All 12 worker receipts written to $RECEIPTS_DIR."

# 4. Run nixcache-builder promote
echo ">>> Executing nixcache-builder promote across all receipts..."
export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="concurrency-test/cache"
export GITHUB_TOKEN="dummy-token"

"$BUILDER_BIN" promote --receipts-dir "$RECEIPTS_DIR"

# 5. Verify merged cache-index in OCI registry
echo ">>> Fetching and verifying merged cache-index manifest and blob..."
MANIFEST_JSON=$(curl -fs -H "Accept: application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/concurrency-test/cache/nix-cache/manifests/cache-index")
BLOB_DIGEST=$(echo "$MANIFEST_JSON" | python3 -c "import sys, json; print(json.load(sys.stdin)['layers'][0]['digest'])")
BLOB_SAFE_NAME=$(echo "$BLOB_DIGEST" | tr ':' '_')
INDEX_JSON=$(cat "/tmp/mock-oci-registry/blobs/$BLOB_SAFE_NAME")

# Total unique packages expected: 1 shared-libc + 4 sys-base + 12 worker pkgs = 17 entries
ENTRY_COUNT=$(echo "$INDEX_JSON" | python3 -c "import sys, json; print(len(json.load(sys.stdin)['entries']))")
echo ">>> Merged cache index contains $ENTRY_COUNT entries."

if [[ "$ENTRY_COUNT" -ne 17 ]]; then
    echo "!!! Expected 17 entries in merged index, but got: $ENTRY_COUNT"
    exit 1
fi
echo ">>> Entry count and deduplication verified (17 unique entries)."

# 6. Verify GC Roots per architecture
echo ">>> Verifying GC roots aggregated per system architecture..."
python3 -c "
import json
with open('/tmp/mock-oci-registry/blobs/$BLOB_SAFE_NAME') as f:
    index = json.load(f)

gc_roots = index['gc_roots']
for sys in ['x86_64-linux', 'aarch64-linux', 'x86_64-darwin', 'aarch64-darwin']:
    assert sys in gc_roots, f'Missing gc_roots for {sys}'
    roots = gc_roots[sys]
    # Each system should have shared-libc, sys-base, and 3 worker pkgs (12 / 4 = 3) -> 5 roots
    assert len(roots) == 5, f'Expected 5 roots for {sys}, got {len(roots)}: {roots}'
    assert 'hash-shared-libc' in roots
    assert f'hash-{sys}-base' in roots
"
echo ">>> GC roots aggregation per system architecture verified."

# 7. Test Merge Idempotency (re-running promote produces identical entry count and no corruption)
echo ">>> Testing merge idempotency by running promote again..."
"$BUILDER_BIN" promote --receipts-dir "$RECEIPTS_DIR"

MANIFEST_JSON_2=$(curl -fs -H "Accept: application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/concurrency-test/cache/nix-cache/manifests/cache-index")
BLOB_DIGEST_2=$(echo "$MANIFEST_JSON_2" | python3 -c "import sys, json; print(json.load(sys.stdin)['layers'][0]['digest'])")
BLOB_SAFE_NAME_2=$(echo "$BLOB_DIGEST_2" | tr ':' '_')
INDEX_JSON_2=$(cat "/tmp/mock-oci-registry/blobs/$BLOB_SAFE_NAME_2")
ENTRY_COUNT_2=$(echo "$INDEX_JSON_2" | python3 -c "import sys, json; print(len(json.load(sys.stdin)['entries']))")

if [[ "$ENTRY_COUNT_2" -ne 17 ]]; then
    echo "!!! Idempotency failed: Expected 17 entries, got $ENTRY_COUNT_2"
    exit 1
fi
echo ">>> Idempotency verified."

# 8. Test Multi-Arch GC dry run
echo ">>> Testing multi-arch GC dry-run against merged index..."
"$BUILDER_BIN" gc --retention-days 30 --dry-run

echo "=== CONCURRENCY MERGE & GC TESTS PASSED SUCCESSFULLY ==="
