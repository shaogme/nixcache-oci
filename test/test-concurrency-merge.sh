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

sys_codes = {
    'x86_64-linux': '11111111',
    'aarch64-linux': '22222222',
    'x86_64-darwin': '33333333',
    'aarch64-darwin': '44444444'
}

hash_shared = '00000000000000000000000000000099'
hash_base = f'000000000000000000000000{sys_codes[sys_name]}'
hash_worker = f'000000000000000000000000000000{worker_id:02d}'

def make_entry(h, name, size):
    return {
        'name': name,
        'system': sys_name,
        'narinfo_meta': {
            'store_path': f'/nix/store/{h}-{name}',
            'nar_basename': f'{name}.nar.xz',
            'compression': 'xz',
            'file_hash': 'sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0',
            'file_size': size,
            'nar_hash': 'sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0',
            'references': [],
            'deriver': None,
            'signatures': ['test-concurrency-key:AAAA='],
            'ca': None
        },
        'nar_digest': 'sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0',
        'nar_size': size,
        'added': '2026-08-28T00:00:00Z',
        'origin_job': None
    }

entries = {
    hash_shared: make_entry(hash_shared, 'glibc-common', 5000),
    hash_base: make_entry(hash_base, f'{sys_name}-base', 10000),
    hash_worker: make_entry(hash_worker, f'pkg-worker-{worker_id}', 2000),
}

active_roots = [
    hash_shared,
    hash_base,
    hash_worker,
]

receipt = {
    'version': 4,
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
        'total_bytes_uploaded': 17000,
        'substituted_paths': 0
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
echo ">>> Fetching and verifying merged cache-index manifest and blobs..."
MANIFEST_INDEX_JSON=$(curl -fs -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/concurrency-test/cache/nix-cache/manifests/cache-index")

python3 -c "
import json, subprocess, sys

manifest_index = json.loads('''$MANIFEST_INDEX_JSON''')
manifests = manifest_index.get('manifests', [])
assert len(manifests) >= 4, f'Expected at least 4 architecture manifests, got {len(manifests)}'

all_entries = {}
gc_roots = {}

for m in manifests:
    sub_manifest_digest = m['digest']
    sub_safe = sub_manifest_digest.replace(':', '_')
    with open(f'/tmp/mock-oci-registry/manifests/{sub_safe}', 'rb') as f:
        sub_manifest = json.load(f)
    
    layer_digest = sub_manifest['layers'][0]['digest']
    layer_safe = layer_digest.replace(':', '_')
    blob_path = f'/tmp/mock-oci-registry/blobs/{layer_safe}'
    
    # Decompress zstd blob
    decompressed = subprocess.check_output(['zstd', '-dc', blob_path])
    arch_data = json.loads(decompressed)
    assert arch_data['version'] == 4, f'Expected version 4, got {arch_data[\"version\"]}'
    
    sys_name = arch_data['system']
    gc_roots[sys_name] = arch_data['gc_roots']
    for k, v in arch_data['entries'].items():
        all_entries[k] = v

print(f'>>> Merged cache index contains {len(all_entries)} unique entries across all architectures.')
assert len(all_entries) == 17, f'Expected 17 entries, got {len(all_entries)}'

for sys_name in ['x86_64-linux', 'aarch64-linux', 'x86_64-darwin', 'aarch64-darwin']:
    assert sys_name in gc_roots, f'Missing gc_roots for {sys_name}'
    roots = gc_roots[sys_name]
    assert len(roots) == 5, f'Expected 5 roots for {sys_name}, got {len(roots)}: {roots}'
    assert '00000000000000000000000000000099' in roots
"
echo ">>> Entry count, deduplication and GC roots aggregation verified."

# 7. Test Merge Idempotency (re-running promote produces identical entry count and no corruption)
echo ">>> Testing merge idempotency by running promote again..."
"$BUILDER_BIN" promote --receipts-dir "$RECEIPTS_DIR"

MANIFEST_INDEX_JSON_2=$(curl -fs -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/concurrency-test/cache/nix-cache/manifests/cache-index")

python3 -c "
import json, subprocess, sys

manifest_index = json.loads('''$MANIFEST_INDEX_JSON_2''')
manifests = manifest_index.get('manifests', [])
assert len(manifests) >= 4, f'Expected at least 4 architecture manifests, got {len(manifests)}'

all_entries = {}
for m in manifests:
    sub_manifest_digest = m['digest']
    sub_safe = sub_manifest_digest.replace(':', '_')
    with open(f'/tmp/mock-oci-registry/manifests/{sub_safe}', 'rb') as f:
        sub_manifest = json.load(f)
    
    layer_digest = sub_manifest['layers'][0]['digest']
    layer_safe = layer_digest.replace(':', '_')
    blob_path = f'/tmp/mock-oci-registry/blobs/{layer_safe}'
    
    decompressed = subprocess.check_output(['zstd', '-dc', blob_path])
    arch_data = json.loads(decompressed)
    for k, v in arch_data['entries'].items():
        all_entries[k] = v

assert len(all_entries) == 17, f'Expected 17 entries on idempotency check, got {len(all_entries)}'
"
echo ">>> Idempotency verified."

# 8. Test Multi-Arch GC dry run
echo ">>> Testing multi-arch GC dry-run against merged index..."
"$BUILDER_BIN" gc --retention-days 30 --dry-run

echo "=== CONCURRENCY MERGE & GC TESTS PASSED SUCCESSFULLY ==="
