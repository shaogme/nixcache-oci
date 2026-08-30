#!/usr/bin/env bash
# test-purge-cas.sh — End-to-end integration test for Cache Purge & CAS Invalidation Workflow

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Starting NixCache Purge & Invalidation CAS Integration Test ==="

TMP_DIR=$(mktemp -d /tmp/nixcache-purge-test-XXXXXX)
export GITHUB_ENV="$TMP_DIR/github_env"
export GITHUB_OUTPUT="$TMP_DIR/github_output"
export GITHUB_PATH="$TMP_DIR/github_path"
touch "$GITHUB_ENV" "$GITHUB_OUTPUT" "$GITHUB_PATH"
unset NIX_CONFIG || true

REGISTRY_PORT=5015
REGISTRY_PID=""

cleanup() {
    echo ">>> Cleaning up test resources..."
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/mock-oci-registry-purge "$TMP_DIR"
    echo ">>> Cleanup complete."
}
trap cleanup EXIT

# 1. Start clean Mock Registry
echo ">>> Launching mock OCI registry on port ${REGISTRY_PORT}..."
pkill -9 -f "mock_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
rm -rf /tmp/mock-oci-registry-purge
mkdir -p /tmp/mock-oci-registry-purge

python3 "$SCRIPT_DIR/mock_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 2. Build nixcache-builder binary
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

    echo ">>> No pre-compiled binaries found. Building nixcache-builder binary..."
    cargo build --bin nixcache-builder
    BUILDER_BIN="./target/debug/nixcache-builder"
}

find_binaries

export NIXCACHE_REPO="testorg/testrepo"
export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export GITHUB_TOKEN="dummy-token"

# 3. Promote a dummy receipt to establish initial baseline cache-index
RECEIPT_DIR="/tmp/mock-oci-registry-purge/receipts"
mkdir -p "$RECEIPT_DIR"
# Create dummy blobs in mock registry
mkdir -p /tmp/mock-oci-registry-purge/blobs
touch /tmp/mock-oci-registry-purge/blobs/sha256_0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0
touch /tmp/mock-oci-registry-purge/blobs/sha256_1d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0
touch /tmp/mock-oci-registry-purge/blobs/sha256_2d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0

cat << 'RECEIPT_JSON' > "$RECEIPT_DIR/receipt-x86.json"
{
  "version": 5,
  "system": "x86_64-linux",
  "repo": "testorg/testrepo",
  "timestamp": "2026-08-29T10:00:00Z",
  "new_entries": {
    "0000000000000000000000000000app1": {
      "name": "my-app-1.0",
      "system": "x86_64-linux",
      "narinfo_meta": {
        "store_path": "/nix/store/0000000000000000000000000000app1-my-app-1.0",
        "nar_basename": "app1.nar.xz",
        "nar_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "nar_size": 500,
        "references": ["0000000000000000000000000000lib1-my-lib-1.0"],
        "signatures": []
      },
      "nar_digest": "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
      "nar_size": 500,
      "added": "2026-08-29T10:00:00Z",
      "origin_job": "run:100:job:x86"
    },
    "0000000000000000000000000000lib1": {
      "name": "my-lib-1.0",
      "system": "x86_64-linux",
      "narinfo_meta": {
        "store_path": "/nix/store/0000000000000000000000000000lib1-my-lib-1.0",
        "nar_basename": "lib1.nar.xz",
        "nar_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "nar_size": 300,
        "references": ["0000000000000000000000000000car1-glibc-2.38"],
        "signatures": []
      },
      "nar_digest": "sha256:1d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
      "nar_size": 300,
      "added": "2026-08-29T10:00:00Z",
      "origin_job": "run:100:job:x86"
    },
    "0000000000000000000000000000car1": {
      "name": "glibc-2.38",
      "system": "x86_64-linux",
      "narinfo_meta": {
        "store_path": "/nix/store/0000000000000000000000000000car1-glibc-2.38",
        "nar_basename": "car1.nar.xz",
        "nar_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "nar_size": 200,
        "references": [],
        "signatures": []
      },
      "nar_digest": "sha256:2d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0",
      "nar_size": 200,
      "added": "2026-08-29T10:00:00Z",
      "origin_job": "run:100:job:x86"
    }
  },
  "active_gc_roots": [
    "0000000000000000000000000000app1"
  ],
  "stats": {
    "discovered_outputs": 1,
    "built_paths": 3,
    "substituted_paths": 0,
    "uploaded_blobs": 3,
    "total_bytes_uploaded": 1000
  }
}
RECEIPT_JSON

echo ">>> Promoting initial receipt to establish baseline index..."
"$BUILDER_BIN" promote --receipt "$RECEIPT_DIR/receipt-x86.json" --target-tag cache-index

# 4. Test Dry-Run Purge for lib1 with Cascade Dependents
echo ">>> Testing Purge in Dry-Run mode for *lib1*..."
"$BUILDER_BIN" purge --patterns "*lib1*" --cascade dependents --dry-run

# 5. Test Actual Purge for lib1 with Cascade Dependents
echo ">>> Executing actual Purge for *lib1* with cascade dependents and strict mode..."
"$BUILDER_BIN" purge --patterns "*lib1*" --cascade dependents --delete-blobs

# 6. Test Purge All to reset baseline
echo ">>> Testing Purge All to clear baseline..."
"$BUILDER_BIN" purge --all

echo "=== NixCache Purge & Invalidation CAS Integration Test PASSED Successfully! ==="
