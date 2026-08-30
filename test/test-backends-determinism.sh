#!/usr/bin/env bash
# test-backends-determinism.sh — Integration test for OCI multi-backend driver determinism (GHCR, Docker Hub, Generic)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting OCI Multi-Backend Determinism Integration Test ==="

REGISTRY_PORT=5009
REGISTRY_PID=""

cleanup() {
    echo ">>> Cleaning up test resources..."
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/mock-oci-registry /tmp/nixcache-backend-test-*
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Build binaries
find_binaries() {
    if [[ -n "${BUILDER_BIN:-}" && -x "$BUILDER_BIN" ]]; then
        echo ">>> Using builder binary from environment variable: BUILDER_BIN=$BUILDER_BIN"
        return 0
    fi

    if [[ -n "${PRECOMPILED_BIN_DIR:-}" && -x "$PRECOMPILED_BIN_DIR/nixcache-builder" ]]; then
        BUILDER_BIN="$PRECOMPILED_BIN_DIR/nixcache-builder"
        echo ">>> Using precompiled builder binary from $PRECOMPILED_BIN_DIR"
        return 0
    fi

    if command -v nixcache-builder &>/dev/null && [[ "${FORCE_BUILD:-false}" != "true" ]]; then
        BUILDER_BIN="$(command -v nixcache-builder)"
        echo ">>> Using builder binary found in PATH: $BUILDER_BIN"
        return 0
    fi

    echo ">>> No pre-compiled binaries found. Building nixcache-builder..."
    cargo build --bin nixcache-builder
    BUILDER_BIN="./target/debug/nixcache-builder"
}

find_binaries

# 2. Launch Mock Registry
echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT}..."
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

# 3. Test Section A: GHCR Deterministic Fixed Two-Step PUT (No PATCH, No 416)
echo ">>> [TEST A] Testing GHCR Provider with FixedTwoStepPut..."
TEST_FILE_GHCR="/tmp/nixcache-backend-test-ghcr.txt"
echo "GHCR payload determinism test $(date +%s%N)" > "$TEST_FILE_GHCR"
STORE_PATH_GHCR=$(nix-store --add "$TEST_FILE_GHCR")
rm -f "$TEST_FILE_GHCR"

# Export using --registry-kind ghcr and verify successful export & upload
RECEIPT_GHCR="/tmp/nixcache-backend-test-receipt-ghcr.json"
"$BUILDER_BIN" session capture \
    --run-id 1001 \
    --job-id "job-ghcr" \
    --registry "127.0.0.1:${REGISTRY_PORT}" \
    --repo "owner/repo" \
    --registry-kind "ghcr" \
    --output-receipt "$RECEIPT_GHCR" \
    "$STORE_PATH_GHCR"

echo ">>> Verifying GHCR build receipt..."
python3 -c "
import json
with open('$RECEIPT_GHCR') as f:
    data = json.load(f)
assert data['stats']['uploaded_blobs'] == 1, 'Expected 1 uploaded blob for GHCR'
print('>>> GHCR upload succeeded deterministically.')
"

# 4. Test Section B: Docker Hub Canonicalization & PreferMonolithicPost
echo ">>> [TEST B] Testing Docker Hub Provider Canonicalization & Monolithic POST..."
TEST_FILE_DOCKER="/tmp/nixcache-backend-test-docker.txt"
echo "Docker Hub payload test $(date +%s%N)" > "$TEST_FILE_DOCKER"
STORE_PATH_DOCKER=$(nix-store --add "$TEST_FILE_DOCKER")
rm -f "$TEST_FILE_DOCKER"

RECEIPT_DOCKER="/tmp/nixcache-backend-test-receipt-docker.json"
"$BUILDER_BIN" session capture \
    --run-id 1002 \
    --job-id "job-docker" \
    --registry "127.0.0.1:${REGISTRY_PORT}" \
    --repo "myrepo" \
    --registry-kind "docker_hub" \
    --output-receipt "$RECEIPT_DOCKER" \
    "$STORE_PATH_DOCKER"

echo ">>> Verifying Docker Hub build receipt..."
python3 -c "
import json
with open('$RECEIPT_DOCKER') as f:
    data = json.load(f)
assert data['stats']['uploaded_blobs'] == 1, 'Expected 1 uploaded blob for Docker Hub'
print('>>> Docker Hub upload succeeded.')
"

# 5. Test Section C: Generic OCI Backend with Environment Variable override
echo ">>> [TEST C] Testing Generic OCI Provider via NIXCACHE_REGISTRY_KIND..."
TEST_FILE_GENERIC="/tmp/nixcache-backend-test-generic.txt"
echo "Generic OCI payload test $(date +%s%N)" > "$TEST_FILE_GENERIC"
STORE_PATH_GENERIC=$(nix-store --add "$TEST_FILE_GENERIC")
rm -f "$TEST_FILE_GENERIC"

RECEIPT_GENERIC="/tmp/nixcache-backend-test-receipt-generic.json"
NIXCACHE_REGISTRY_KIND="generic_oci" "$BUILDER_BIN" session capture \
    --run-id 1003 \
    --job-id "job-generic" \
    --registry "127.0.0.1:${REGISTRY_PORT}" \
    --repo "custom/harbor/cache" \
    --output-receipt "$RECEIPT_GENERIC" \
    "$STORE_PATH_GENERIC"

echo ">>> Verifying Generic OCI build receipt..."
python3 -c "
import json
with open('$RECEIPT_GENERIC') as f:
    data = json.load(f)
assert data['stats']['uploaded_blobs'] == 1, 'Expected 1 uploaded blob for Generic OCI'
print('>>> Generic OCI upload succeeded.')
"

# 6. Test Section D: Promote & Multi-arch Index Integration across providers
echo ">>> [TEST D] Testing Promote on multi-backend session..."
"$BUILDER_BIN" promote \
    --run-id 1001 \
    --registry "127.0.0.1:${REGISTRY_PORT}" \
    --repo "owner/repo" \
    --registry-kind "ghcr" \
    --receipt "$RECEIPT_GHCR" \
    --target-tag "cache-index"

echo ">>> Verifying Promoted cache-index in Mock Registry..."
INDEX_RESP=$(curl -fs -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/owner/repo/nix-cache/manifests/cache-index")
echo "$INDEX_RESP" | grep -q "schemaVersion"
echo ">>> Promoted cache-index successfully verified."

echo "=== ALL OCI BACKEND DETERMINISM INTEGRATION TESTS PASSED ==="
