#!/usr/bin/env bash
# test-sharding-scale-simulation.sh — Scale & Concurrency Stress Test Suite (100k ~ 1M Entries)
# Verifies Radix Prefix Sharding distribution, Merkle Tree state integrity & high-concurrency simulation.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting NixCache Schema v5 Sharding Scale & Concurrency Stress Test Suite ==="

TMP_DIR=$(mktemp -d /tmp/nixcache-scale-test-XXXXXX)
export GITHUB_ENV="$TMP_DIR/github_env"
export GITHUB_OUTPUT="$TMP_DIR/github_output"
export GITHUB_PATH="$TMP_DIR/github_path"
touch "$GITHUB_ENV" "$GITHUB_OUTPUT" "$GITHUB_PATH"
unset NIX_CONFIG || true

REGISTRY_PORT=5012
REGISTRY_PID=""

cleanup() {
    echo ">>> Cleaning up scale test resources..."
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/mock-oci-registry "$TMP_DIR"
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. 解析参数确定规模
SCALE_ENTRIES="${SCALE_ENTRIES:-100000}"
CONCURRENCY="${CONCURRENCY:-64}"

for arg in "$@"; do
    case "$arg" in
        --full|--million|1000000|1M|1m)
            SCALE_ENTRIES=1000000
            ;;
        --quick|10000|10k)
            SCALE_ENTRIES=10000
            ;;
        [0-9]*)
            SCALE_ENTRIES="$arg"
            ;;
    esac
done

echo ">>> Target test scale: ${SCALE_ENTRIES} entries, Concurrency: ${CONCURRENCY} workers."

# 2. 编译压测与仿真套件二进制
find_sim_binary() {
    if [[ -n "${SIM_BIN:-}" && -x "$SIM_BIN" ]]; then
        echo ">>> Using simulation binary from environment variable: SIM_BIN=$SIM_BIN"
        return 0
    fi

    if [[ "$SCALE_ENTRIES" -ge 500000 ]]; then
        echo ">>> Compiling sharding-scale-sim in --release mode for large scale (${SCALE_ENTRIES} entries)..."
        cargo build --release --bin sharding-scale-sim
        SIM_BIN="./target/release/sharding-scale-sim"
    else
        echo ">>> Compiling sharding-scale-sim in debug mode..."
        cargo build --bin sharding-scale-sim
        SIM_BIN="./target/debug/sharding-scale-sim"
    fi
}

find_sim_binary

# 3. 运行 10万 ~ 100万规模核心仿真与检验套件 (输出人类可读报告与 JSON)
echo ">>> Executing full-scale Rust simulation and verification suite..."
"$SIM_BIN" -n "$SCALE_ENTRIES" -c "$CONCURRENCY" --system "x86_64-linux"

JSON_OUTPUT_FILE="$TMP_DIR/scale-report.json"
"$SIM_BIN" -n "$SCALE_ENTRIES" -c "$CONCURRENCY" --system "x86_64-linux" --json > "$JSON_OUTPUT_FILE"

python3 -c "
import json, sys

with open('$JSON_OUTPUT_FILE', 'r') as f:
    report = json.load(f)

assert report['all_checks_passed'] == True, 'Report marked all_checks_passed as False'
assert report['distribution']['passed_uniformity_check'] == True, 'Distribution uniformity check failed'
assert report['distribution']['empty_shards_count'] == 0 or report['scale_entries'] < 100000, f'Unexpected empty shards: {report[\"distribution\"][\"empty_shards_count\"]}'
assert report['merkle_determinism_passed'] == True, 'Merkle determinism failed'
assert report['merkle_tamper_detection_passed'] == True, 'Merkle tamper detection failed'
assert report['incremental_diff']['diff_matches_exact'] == True, 'Incremental diff mismatch'
assert report['bloom_filter']['passed_scale_check'] == True, 'Bloom filter scale check failed'
assert report['bloom_filter']['false_negatives_count'] == 0, 'Bloom filter false negatives detected'
assert report['bloom_filter']['false_positive_rate'] <= 0.018, f'Bloom filter FPR too high: {report[\"bloom_filter\"][\"false_positive_rate\"]}'
assert report['concurrent_read']['throughput_qps'] > 500000, f'Read throughput too low: {report[\"concurrent_read\"][\"throughput_qps\"]}'
assert report['concurrent_write_compaction']['compaction_passed'] == True, 'Compaction simulation failed'
assert report['concurrent_write_compaction']['write_amplification_reduction_pct'] >= 70.0, f'Write amplification reduction too low: {report[\"concurrent_write_compaction\"][\"write_amplification_reduction_pct\"]}'

print(f'>>> JSON Metrics Verified: Scale={report[\"scale_entries\"]}, Read QPS={report[\"concurrent_read\"][\"throughput_qps\"]:.0f}, Bloom FPR={report[\"bloom_filter\"][\"false_positive_rate\"]*100:.3f}%, Write Amplification Reduction={report[\"concurrent_write_compaction\"][\"write_amplification_reduction_pct\"]:.2f}%')
"

# 4. 启动 Mock OCI Registry 验证端到端分片索引发布与 Partial Compaction
echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT} for E2E Sharding verification..."
pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
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

# 5. 编译 nixcache-builder
find_builder_binary() {
    if [[ -n "${BUILDER_BIN:-}" && -x "$BUILDER_BIN" ]]; then
        echo ">>> Using builder binary from environment variable: BUILDER_BIN=$BUILDER_BIN"
        return 0
    fi

    if command -v nixcache-builder &>/dev/null && [[ "${FORCE_BUILD:-false}" != "true" ]]; then
        BUILDER_BIN="$(command -v nixcache-builder)"
        echo ">>> Using builder binary found in PATH: $BUILDER_BIN"
        return 0
    fi

    echo ">>> Building nixcache-builder..."
    cargo build --bin nixcache-builder
    BUILDER_BIN="./target/debug/nixcache-builder"
}

find_builder_binary

# 6. 生成 10,000 条真实测试 Receipt 并执行 Promote 局部压实验证
echo ">>> Simulating large receipt generation and OCI Promote Compaction..."
RECEIPTS_DIR="$TMP_DIR/receipts"
mkdir -p "$RECEIPTS_DIR"

python3 - "$RECEIPTS_DIR" << 'PYEOF'
import json, sys, hashlib

receipts_dir = sys.argv[1]
base32_chars = "0123456789abcdfghijklmnpqrsvwxyz"

def make_hash(i):
    h = hashlib.sha256(f"pkg-scale-e2e-{i}".encode()).hexdigest()
    # 构造 32 字符 base32
    res = []
    for k in range(32):
        idx = int(h[k % len(h)], 16) + (k % 16)
        res.append(base32_chars[idx % 32])
    return "".join(res)

for batch_id in range(5):
    entries = {}
    roots = []
    for i in range(1000):
        idx = batch_id * 1000 + i
        h = make_hash(idx)
        name = f"scale-pkg-{idx}"
        entries[h] = {
            'name': name,
            'system': 'x86_64-linux',
            'narinfo_meta': {
                'store_path': f'/nix/store/{h}-{name}',
                'nar_basename': f'{name}.nar.xz',
                'compression': 'xz',
                'file_hash': 'sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0',
                'file_size': 4096,
                'nar_hash': 'sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0',
                'references': [],
                'deriver': None,
                'signatures': ['test-scale-key:AAAA='],
                'ca': None
            },
            'nar_digest': 'sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0',
            'nar_size': 4096,
            'added': '2026-08-30T00:00:00Z',
            'origin_job': f'batch-{batch_id}'
        }
        roots.append(h)

    receipt = {
        'version': 5,
        'system': 'x86_64-linux',
        'repo': 'scale-test/cache',
        'timestamp': '2026-08-30T00:00:00Z',
        'public_key': 'test-scale-key:AAAA=',
        'new_entries': entries,
        'active_gc_roots': roots,
        'stats': {
            'discovered_outputs': 1000,
            'built_paths': 1000,
            'uploaded_blobs': 1000,
            'total_bytes_uploaded': 4096000,
            'substituted_paths': 0
        }
    }

    with open(f"{receipts_dir}/receipt-batch-{batch_id}.json", "w") as f:
        json.dump(receipt, f)
PYEOF

export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="scale-test/cache"
export GITHUB_TOKEN="dummy-token"

echo ">>> Executing nixcache-builder promote across 5,000 entries..."
"$BUILDER_BIN" promote --receipts-dir "$RECEIPTS_DIR" --target-tag "cache-index"

# 7. 校验 OCI 中的 Sharded Root Index、Bloom Filter 和 Shards
echo ">>> Verifying OCI Sharded Root Index and Bloom Filter Layers..."
MANIFEST_INDEX_JSON=$(curl -fs -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/scale-test/cache/nix-cache/manifests/cache-index")

python3 -c "
import json, subprocess

manifest_index = json.loads('''$MANIFEST_INDEX_JSON''')
sub_manifest_digest = manifest_index['manifests'][0]['digest']
sub_safe = sub_manifest_digest.replace(':', '_')

with open(f'/tmp/mock-oci-registry/manifests/{sub_safe}', 'rb') as f:
    sub_manifest = json.load(f)

assert len(sub_manifest['layers']) == 2, f'Expected 2 layers (Root + Bloom), got {len(sub_manifest[\"layers\"])}'
root_layer = sub_manifest['layers'][0]
bloom_layer = sub_manifest['layers'][1]

assert root_layer['mediaType'] == 'application/vnd.nix.cache.root.v5+zstd'
assert bloom_layer['mediaType'] == 'application/vnd.nix.cache.bloom.v5+zstd'

# Decompress and verify Root Index
blob_path = f'/tmp/mock-oci-registry/blobs/{root_layer[\"digest\"].replace(\":\", \"_\")}'
decompressed = subprocess.check_output(['zstd', '-dc', blob_path])
root_data = json.loads(decompressed)

assert root_data['version'] == 5
assert len(root_data['shards']) == 1024
total_entries = sum(s['entry_count'] for s in root_data['shards'])
assert total_entries == 5000, f'Expected 5000 entries across shards, got {total_entries}'

print(f'>>> E2E Sharding Verified: 5000 entries partitioned across 1024 shards with Merkle Root: {root_data[\"merkle_root\"][:16]}...')
"

echo "=== ALL SHARDING SCALE & CONCURRENCY SIMULATION TESTS PASSED SUCCESSFULLY ==="
