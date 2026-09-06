#!/usr/bin/env bash
# test-security-signature.sh — Test security verification & tampering rejection

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix OCI Cache Security & Signature Verification Test ==="

TMP_DIR=$(mktemp -d /tmp/nixcache-security-test-XXXXXX)
export GITHUB_ENV="$TMP_DIR/github_env"
export GITHUB_OUTPUT="$TMP_DIR/github_output"
export GITHUB_PATH="$TMP_DIR/github_path"
touch "$GITHUB_ENV" "$GITHUB_OUTPUT" "$GITHUB_PATH"
unset NIX_CONFIG || true

REGISTRY_PORT=5001
PROXY_PORT=37515
REGISTRY_PID=""
PROXY_PID=""

cleanup() {
    echo ">>> Cleaning up security test resources..."
    git checkout -- examples/flake/flake.nix 2>/dev/null || true
    if [[ -n "${PROXY_PID:-}" ]]; then
        kill -9 "$PROXY_PID" 2>/dev/null || true
    fi
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "run_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    podman rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || docker rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/nixcache-test-registry "$TMP_DIR"
    rm -f valid-secret.key valid-public.key rogue-secret.key rogue-public.key
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Generate legitimate and rogue signing key pairs
echo ">>> Generating legitimate and rogue key pairs..."
rm -f valid-secret.key valid-public.key rogue-secret.key rogue-public.key
nix-store --generate-binary-cache-key valid-key-1 valid-secret.key valid-public.key
nix-store --generate-binary-cache-key rogue-key-1 rogue-secret.key rogue-public.key

# 2. Start clean OCI Registry Container
echo ">>> Launching OCI registry container on port ${REGISTRY_PORT}..."
pkill -9 -f "run_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
podman rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || docker rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || true
rm -rf /tmp/nixcache-test-registry
python3 "$SCRIPT_DIR/run_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

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

delete_store_path() {
    local target_path="$1"
    local nix_store_bin
    nix_store_bin=$(command -v nix-store)

    # 1. 尝试以当前用户权限删除
    "$nix_store_bin" --delete "$target_path" --ignore-liveness 2>/dev/null || true

    # 2. 如果仍存在且具备 sudo 权限，以 root 身份强制删除（应对 multi-user nix-daemon 环境）
    if "$nix_store_bin" --query --hash "$target_path" >/dev/null 2>&1; then
        if command -v sudo &>/dev/null && sudo -n true 2>/dev/null; then
            sudo "$nix_store_bin" --delete "$target_path" --ignore-liveness 2>/dev/null || true
        fi
    fi

    # 3. 如果依然有效，先执行 GC 回收（清理临时根）再强制删除
    if "$nix_store_bin" --query --hash "$target_path" >/dev/null 2>&1; then
        if command -v sudo &>/dev/null && sudo -n true 2>/dev/null; then
            sudo nix-collect-garbage 2>/dev/null || true
            sudo "$nix_store_bin" --delete "$target_path" --ignore-liveness 2>/dev/null || true
        else
            nix-collect-garbage 2>/dev/null || true
            "$nix_store_bin" --delete "$target_path" --ignore-liveness 2>/dev/null || true
        fi
    fi

    # 4. 严格断言：确保路径已被移出本地 store 数据库
    if "$nix_store_bin" --query --hash "$target_path" >/dev/null 2>&1; then
        echo "!!! CRITICAL: Failed to evict $target_path from local Nix store before test!"
        exit 1
    fi
}

# 4. Build a test package and publish to mock registry
echo ">>> Building package with legitimate signature..."
export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export NIXCACHE_REPO="security-test/cache"
export NIXCACHE_SIGNING_KEY_FILE="valid-secret.key"
export GITHUB_TOKEN="dummy-token"

sed -i "s/Built at: .*/Built at: $(date +%s%N)\"/" examples/flake/flake.nix
TEST_STORE_PATH=$(nix build "./examples/flake#nixcache-test" --no-link --print-out-paths)
TEST_HASH=$(basename "$TEST_STORE_PATH" | cut -d'-' -f1)

echo ">>> Store path: $TEST_STORE_PATH (Hash: $TEST_HASH)"

# Run builder build + promote to sign and upload
export NIXCACHE_MODE="flake"
export NIXCACHE_CONFIG_DIR="examples/flake"
RECEIPT_FILE="$(mktemp --suffix=.json)"
PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN" build --output-receipt "$RECEIPT_FILE"
"$BUILDER_BIN" promote --receipt "$RECEIPT_FILE"
rm -f "$RECEIPT_FILE"

# 5. Start proxy
echo ">>> Starting nixcache-proxy..."
export NIXCACHE_LISTEN="127.0.0.1"
export NIXCACHE_PORT="${PROXY_PORT}"
export NIXCACHE_UPSTREAM=""
unset NIXCACHE_INDEX_DIR
unset CACHE_DIRECTORY

"$PROXY_BIN" &
PROXY_PID=$!

for _ in {1..15}; do
    if curl -fs "http://127.0.0.1:${PROXY_PORT}/nix-cache-info" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 6. Test Security Scenario 1: Untrusted Public Key (Signature verification must fail)
echo ">>> Security Test 1: Verifying that Nix rejects substitution when signed by untrusted key..."
delete_store_path "$TEST_STORE_PATH"

if nix-store --realise "$TEST_STORE_PATH" \
    --max-jobs 0 \
    --option substituters "http://127.0.0.1:${PROXY_PORT}" \
    --option trusted-public-keys "$(cat rogue-public.key)" \
    --option require-sigs true 2>/dev/null; then
    echo "!!! SECURITY FAILURE: Nix accepted package signed with untrusted key!"
    exit 1
else
    echo ">>> PASS: Nix correctly rejected untrusted signature."
fi

# 7. Test Security Scenario 2: Tampered Blob Rejection (Hash mismatch must fail)
echo ">>> Security Test 2: Tampering with cached blob contents in OCI registry..."
for blob_file in /tmp/nixcache-test-registry/docker/registry/v2/blobs/sha256/*/*/data; do
    if [[ -f "$blob_file" ]] && [[ $(wc -c < "$blob_file") -gt 100 ]]; then
        echo "Corrupting blob: $blob_file"
        echo "CORRUPTED_PAYLOAD_TAMPERED_CONTENT" > "$blob_file"
    fi
done

delete_store_path "$TEST_STORE_PATH"

if nix-store --realise "$TEST_STORE_PATH" \
    --max-jobs 0 \
    --option substituters "http://127.0.0.1:${PROXY_PORT}" \
    --option trusted-public-keys "$(cat valid-public.key)" \
    --option require-sigs true 2>/dev/null; then
    echo "!!! SECURITY FAILURE: Nix accepted corrupted / tampered NAR blob!"
    exit 1
else
    echo ">>> PASS: Nix correctly detected tampering and rejected corrupted NAR blob."
fi

# 8. Test Security Scenario 3: Valid Untampered Substitution
echo ">>> Security Test 3: Verifying successful substitution with authentic package..."
# Clean registry and re-push pristine package
kill -9 "$PROXY_PID" 2>/dev/null || true
kill -9 "$REGISTRY_PID" 2>/dev/null || true
pkill -9 -f "run_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
podman rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || docker rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || true
rm -rf /tmp/nixcache-test-registry

python3 "$SCRIPT_DIR/run_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!
for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

RECEIPT_FILE="$(mktemp --suffix=.json)"
PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN" build --output-receipt "$RECEIPT_FILE"
"$BUILDER_BIN" promote --receipt "$RECEIPT_FILE"
rm -f "$RECEIPT_FILE"

"$PROXY_BIN" &
PROXY_PID=$!
sleep 1

delete_store_path "$TEST_STORE_PATH"

nix-store --realise "$TEST_STORE_PATH" \
    --max-jobs 0 \
    --option substituters "http://127.0.0.1:${PROXY_PORT}" \
    --option trusted-public-keys "$(cat valid-public.key)" \
    --option require-sigs true

if [[ -x "$TEST_STORE_PATH/bin/nixcache-test" ]]; then
    "$TEST_STORE_PATH/bin/nixcache-test"
    echo ">>> PASS: Valid package substituted and verified successfully."
else
    echo "!!! Valid package could not be executed!"
    exit 1
fi

echo "=== SECURITY & SIGNATURE TESTS PASSED SUCCESSFULLY ==="
