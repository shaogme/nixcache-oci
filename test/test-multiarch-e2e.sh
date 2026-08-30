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
    if [[ -n "${PROXY_ARM_PID:-}" ]]; then
        kill -9 "$PROXY_ARM_PID" 2>/dev/null || true
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

    echo ">>> No pre-compiled binaries found. Building cargo workspace..."
    cargo build --workspace
    BUILDER_BIN="./target/debug/nixcache-builder"
    PROXY_BIN="./target/debug/nixcache-proxy"
}

find_binaries

export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="test/multiarch"
export NIXCACHE_SIGNING_KEY_FILE="test-multi-secret.key"
export GITHUB_TOKEN="dummy-token"
export NIXCACHE_PROXY_BIN="$PROXY_BIN"
export PROXY_BIN="$PROXY_BIN"
PROXY_DIR="$(cd "$(dirname "$PROXY_BIN")" && pwd)"
export PATH="$PROXY_DIR:$PATH"
if [[ ! -e "$PROXY_DIR/nixcache-proxy" && -x "$PROXY_BIN" ]]; then
    ln -sf "$PROXY_BIN" "$PROXY_DIR/nixcache-proxy" 2>/dev/null || true
fi
if [[ ! -e "$PROXY_DIR/nixcache-builder" && -x "$BUILDER_BIN" ]]; then
    ln -sf "$BUILDER_BIN" "$PROXY_DIR/nixcache-builder" 2>/dev/null || true
fi


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
    --strict

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
    --strict

rm -f "$CUSTOM_PKG_FILE"

if [[ ! -f "$RECEIPT_2" ]]; then
    echo "!!! Worker 2 failed to generate receipt: $RECEIPT_2"
    exit 1
fi
echo ">>> Worker 2 receipt generated successfully:"
cat "$RECEIPT_2"
echo ""

# =========================================================================
# Phase 2: Gather (Promote Coordinator aggregating Receipts & publishing Index)
# =========================================================================

echo ">>> [Phase 2] Running Promote Coordinator..."
"$BUILDER_BIN" promote \
    --receipts-dir "$RECEIPTS_DIR" \
    --repo "$NIXCACHE_REPO" \
    --registry "$NIXCACHE_REGISTRY"

echo ">>> Fetching published cache-index from local registry to verify Schema v4..."
INDEX_MANIFEST=$(curl -fsSL -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://${NIXCACHE_REGISTRY}/v2/${NIXCACHE_REPO}/nix-cache/manifests/cache-index")

echo ">>> Published Cache Index Manifest:"
echo "$INDEX_MANIFEST" | python3 -m json.tool

# Verify Schema v4 and multi-arch entries
python3 -c "
import json, subprocess, sys

manifest_index = json.loads('''$INDEX_MANIFEST''')
manifests = manifest_index.get('manifests', [])
assert len(manifests) >= 2, f'Expected at least 2 architecture manifests, got {len(manifests)}'

all_entries = {}
gc_roots = {}

for m in manifests:
    sub_manifest_digest = m['digest']
    sub_manifest_json = subprocess.check_output([
        'curl', '-fsSL',
        '-H', 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/manifests/{sub_manifest_digest}'
    ]).decode('utf-8')
    sub_manifest = json.loads(sub_manifest_json)
    layer_digest = sub_manifest['layers'][0]['digest']
    
    blob_bytes = subprocess.check_output([
        'curl', '-fsSL',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/blobs/{layer_digest}'
    ])
    
    decompressed = subprocess.check_output(['zstd', '-dc'], input=blob_bytes)
    arch_data = json.loads(decompressed)
    assert arch_data['version'] == 4, f'Expected version 4, got {arch_data[\"version\"]}'
    
    sys_name = arch_data['system']
    gc_roots[sys_name] = arch_data['gc_roots']
    for k, v in arch_data['entries'].items():
        all_entries[k] = v

print(f'>>> Aggregated {len(all_entries)} entries across {len(gc_roots)} architectures.')
assert len(all_entries) >= 2, f'Expected at least 2 entries, got {len(all_entries)}'
assert 'x86_64-linux' in gc_roots, f'Missing x86_64-linux in gc_roots: {gc_roots}'
assert 'aarch64-linux' in gc_roots, f'Missing aarch64-linux in gc_roots: {gc_roots}'
print('>>> Schema v4 and Multi-Arch verification SUCCESS!')
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

# Fetch x86_64 entries and verify on x86_64 proxy
X86_HASHES=$(python3 -c "
import json, subprocess
manifest_index = json.loads('''$INDEX_MANIFEST''')
manifests = manifest_index.get('manifests', [])
for m in manifests:
    sub_manifest_digest = m['digest']
    sub_manifest_json = subprocess.check_output([
        'curl', '-fsSL',
        '-H', 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/manifests/{sub_manifest_digest}'
    ]).decode('utf-8')
    sub_manifest = json.loads(sub_manifest_json)
    layer_digest = sub_manifest['layers'][0]['digest']
    blob_bytes = subprocess.check_output([
        'curl', '-fsSL',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/blobs/{layer_digest}'
    ])
    decompressed = subprocess.check_output(['zstd', '-dc'], input=blob_bytes)
    arch_data = json.loads(decompressed)
    if arch_data['system'] == 'x86_64-linux':
        for h in arch_data['entries'].keys():
            print(h)
")

for HASH in $X86_HASHES; do
    echo ">>> Testing .narinfo query for x86_64-linux hash: $HASH"
    NARINFO=$(curl -fs "http://127.0.0.1:37516/${HASH}.narinfo")
    echo "$NARINFO"
    if ! echo "$NARINFO" | grep -q "StorePath:"; then
        echo "!!! Invalid narinfo for hash $HASH"
        exit 1
    fi
done

# Start aarch64-linux proxy on port 37517 and verify aarch64-linux entries
echo ">>> Starting nixcache-proxy for aarch64-linux on port 37517..."
"$PROXY_BIN" --port 37517 --system aarch64-linux &
PROXY_ARM_PID=$!

for _ in {1..10}; do
    if curl -fs http://127.0.0.1:37517/nix-cache-info >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

ARM_HASHES=$(python3 -c "
import json, subprocess
manifest_index = json.loads('''$INDEX_MANIFEST''')
manifests = manifest_index.get('manifests', [])
for m in manifests:
    sub_manifest_digest = m['digest']
    sub_manifest_json = subprocess.check_output([
        'curl', '-fsSL',
        '-H', 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/manifests/{sub_manifest_digest}'
    ]).decode('utf-8')
    sub_manifest = json.loads(sub_manifest_json)
    layer_digest = sub_manifest['layers'][0]['digest']
    blob_bytes = subprocess.check_output([
        'curl', '-fsSL',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/blobs/{layer_digest}'
    ])
    decompressed = subprocess.check_output(['zstd', '-dc'], input=blob_bytes)
    arch_data = json.loads(decompressed)
    if arch_data['system'] == 'aarch64-linux':
        for h in arch_data['entries'].keys():
            print(h)
")

for HASH in $ARM_HASHES; do
    echo ">>> Testing .narinfo query for aarch64-linux hash on ARM proxy: $HASH"
    NARINFO=$(curl -fs "http://127.0.0.1:37517/${HASH}.narinfo")
    echo "$NARINFO"
    if ! echo "$NARINFO" | grep -q "StorePath:"; then
        echo "!!! Invalid narinfo for hash $HASH"
        exit 1
    fi
done

kill -9 "$PROXY_ARM_PID" 2>/dev/null || true
unset PROXY_ARM_PID

# Perform full Nix substitution on one of the store paths
SAMPLE_STORE_PATH=$(python3 -c "
import json, subprocess, sys
manifest_index = json.loads('''$INDEX_MANIFEST''')
manifests = manifest_index.get('manifests', [])
for m in manifests:
    sub_manifest_digest = m['digest']
    sub_manifest_json = subprocess.check_output([
        'curl', '-fsSL',
        '-H', 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/manifests/{sub_manifest_digest}'
    ]).decode('utf-8')
    sub_manifest = json.loads(sub_manifest_json)
    layer_digest = sub_manifest['layers'][0]['digest']
    blob_bytes = subprocess.check_output([
        'curl', '-fsSL',
        f'http://$NIXCACHE_REGISTRY/v2/$NIXCACHE_REPO/nix-cache/blobs/{layer_digest}'
    ])
    decompressed = subprocess.check_output(['zstd', '-dc'], input=blob_bytes)
    arch_data = json.loads(decompressed)
    if arch_data['system'] == 'x86_64-linux':
        for entry in arch_data['entries'].values():
            print(entry['narinfo_meta']['store_path'])
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
