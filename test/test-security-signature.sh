#!/usr/bin/env bash
# test-security-signature.sh — Test security verification & tampering rejection

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting Nix OCI Cache Security & Signature Verification Test ==="

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
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/mock-oci-registry
    rm -f valid-secret.key valid-public.key rogue-secret.key rogue-public.key
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Generate legitimate and rogue signing key pairs
echo ">>> Generating legitimate and rogue key pairs..."
rm -f valid-secret.key valid-public.key rogue-secret.key rogue-public.key
nix-store --generate-binary-cache-key valid-key-1 valid-secret.key valid-public.key
nix-store --generate-binary-cache-key rogue-key-1 rogue-secret.key rogue-public.key

# 2. Start clean Mock Registry
echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT}..."
pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
rm -rf /tmp/mock-oci-registry
python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 3. Build builder and proxy binaries
echo ">>> Building cargo workspace..."
cargo build --workspace

BUILDER_BIN="./target/debug/nixcache-builder"
PROXY_BIN="./target/debug/nixcache-proxy"

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
nix-store --delete "$TEST_STORE_PATH" 2>/dev/null || true

if nix-store --realise "$TEST_STORE_PATH" \
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
for blob_file in /tmp/mock-oci-registry/blobs/*; do
    if [[ -f "$blob_file" ]] && [[ $(wc -c < "$blob_file") -gt 100 ]]; then
        echo "Corrupting blob: $blob_file"
        echo "CORRUPTED_PAYLOAD_TAMPERED_CONTENT" >> "$blob_file"
    fi
done

nix-store --delete "$TEST_STORE_PATH" 2>/dev/null || true

if nix-store --realise "$TEST_STORE_PATH" \
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
rm -rf /tmp/mock-oci-registry

python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!
sleep 1

RECEIPT_FILE="$(mktemp --suffix=.json)"
PATH="$(cd "$(dirname "$PROXY_BIN")" && pwd):$PATH" "$BUILDER_BIN" build --output-receipt "$RECEIPT_FILE"
"$BUILDER_BIN" promote --receipt "$RECEIPT_FILE"
rm -f "$RECEIPT_FILE"

"$PROXY_BIN" &
PROXY_PID=$!
sleep 1

nix-store --delete "$TEST_STORE_PATH" 2>/dev/null || true

nix-store --realise "$TEST_STORE_PATH" \
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
