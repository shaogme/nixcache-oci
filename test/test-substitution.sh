#!/usr/bin/env bash
# test-substitution.sh — Integration test using podman to verify OCI-backed cache works
set -euo pipefail

REPO="${1:-shaogme/nixcache-oci}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

echo "=== Nix Binary Cache (OCI) Substitution Test ==="
echo "Repo: $REPO"

# Fetch the cache index from GHCR
echo ">>> Fetching cache index from GHCR..."
CRED_TOKEN=$(gh auth token 2>/dev/null || echo "")
# Exchange for OCI registry token
TOKEN=$(curl -s -u "token:${CRED_TOKEN}" \
    "https://ghcr.io/token?scope=repository:${REPO}/nix-cache:pull&service=ghcr.io" 2>/dev/null \
    | jq -r '.token // empty')
if [[ -z "$TOKEN" ]]; then
    TOKEN="$CRED_TOKEN"
fi

STORE_INFO=$(python3 -c "
import json, subprocess, sys

token = '''$TOKEN'''
repo = '''$REPO'''
headers = ['-H', f'Authorization: Bearer {token}'] if token else []

try:
    manifest_raw = subprocess.check_output([
        'curl', '-fsSL',
        *headers,
        '-H', 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json',
        f'https://ghcr.io/v2/{repo}/nix-cache/manifests/cache-index'
    ])
    manifest_json = json.loads(manifest_raw)

    if 'manifests' in manifest_json:
        sub_digest = manifest_json['manifests'][0]['digest']
        sub_raw = subprocess.check_output([
            'curl', '-fsSL',
            *headers,
            '-H', 'Accept: application/vnd.oci.image.manifest.v1+json',
            f'https://ghcr.io/v2/{repo}/nix-cache/manifests/{sub_digest}'
        ])
        sub_json = json.loads(sub_raw)
    else:
        sub_json = manifest_json

    layer_digest = sub_json['layers'][0]['digest']
    blob_bytes = subprocess.check_output([
        'curl', '-fsSL', '-L',
        *headers,
        f'https://ghcr.io/v2/{repo}/nix-cache/blobs/{layer_digest}'
    ])

    decompressed = subprocess.check_output(['zstd', '-dc'], input=blob_bytes)
    root_data = json.loads(decompressed)

    if 'shards' in root_data:
        for shard in root_data['shards']:
            if shard['entry_count'] > 0 and shard['blob_digest']:
                s_bytes = subprocess.check_output([
                    'curl', '-fsSL', '-L',
                    *headers,
                    f'https://ghcr.io/v2/{repo}/nix-cache/blobs/{shard[\"blob_digest\"]}'
                ])
                s_decomp = subprocess.check_output(['zstd', '-dc'], input=s_bytes)
                s_data = json.loads(s_decomp)
                for h, entry in s_data['entries'].items():
                    print(f'{h} {entry.get(\"name\", \"unknown\")}')
                    sys.exit(0)
    elif 'entries' in root_data:
        for h, entry in root_data['entries'].items():
            print(f'{h} {entry.get(\"name\", \"unknown\")}')
            sys.exit(0)
except Exception as e:
    sys.stderr.write(f'Error fetching index: {e}\n')
    sys.exit(1)
") || {
    echo "!!! Cannot fetch cache index. Has the cache been published?"
    exit 1
}

STORE_HASH=$(echo "$STORE_INFO" | cut -d' ' -f1)
STORE_NAME=$(echo "$STORE_INFO" | cut -d' ' -f2)

if [[ -z "$STORE_HASH" ]]; then
    echo "!!! Index is empty"
    exit 1
fi

echo ">>> Testing: $STORE_HASH-$STORE_NAME"

cat <<'CONTAINER_SCRIPT' > "$PROJECT_DIR/test/run-in-container.sh"
#!/usr/bin/env bash
set -euo pipefail

REPO="$1"
STORE_HASH="$2"

echo "=== Inside container ==="

echo ">>> Installing curl..."
nix-env -iA nixpkgs.curl 2>&1 | tail -3

echo ">>> Building proxy..."
nix build /workspace#cache-proxy --profile /tmp/proxy-profile

echo ">>> Starting proxy..."
NIXCACHE_REPO="$REPO" /tmp/proxy-profile/bin/nixcache-proxy &
PROXY_PID=$!
sleep 3

if ! kill -0 $PROXY_PID 2>/dev/null; then
    echo "!!! Proxy failed to start"
    exit 1
fi

echo ">>> Testing /nix-cache-info..."
CACHE_INFO=$(curl -fs --max-time 10 http://localhost:37515/nix-cache-info)
echo "$CACHE_INFO"

# Wait for index to load (narinfo lookups need it)
echo ">>> Waiting for index to load..."
for i in $(seq 1 30); do
    if curl -fs --max-time 5 "http://localhost:37515/${STORE_HASH}.narinfo" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

echo ">>> Testing narinfo lookup for $STORE_HASH..."
NARINFO=$(curl -fs --max-time 15 "http://localhost:37515/${STORE_HASH}.narinfo") || {
    echo "!!! narinfo lookup failed"
    kill $PROXY_PID 2>/dev/null; exit 1
}
echo "$NARINFO"

STORE_PATH=$(echo "$NARINFO" | grep '^StorePath: ' | cut -d' ' -f2)
echo ">>> Full store path: $STORE_PATH"

mkdir -p /etc/nix
cat > /etc/nix/nix.conf <<EOF
substituters = http://localhost:37515
trusted-substituters = http://localhost:37515
require-sigs = false
sandbox = false
experimental-features = nix-command flakes
EOF

echo ">>> Realising $STORE_PATH from cache..."
nix-store --realise "$STORE_PATH" 2>&1 || {
    echo "!!! Failed to realise store path"
    kill $PROXY_PID 2>/dev/null; exit 1
}

if [[ -e "$STORE_PATH" ]]; then
    echo ">>> SUCCESS: $STORE_PATH exists!"
    if [[ -d "$STORE_PATH/bin" ]]; then
        FIRST_BIN=$(ls "$STORE_PATH/bin/" | head -1)
        echo ">>> Running $FIRST_BIN:"
        "$STORE_PATH/bin/$FIRST_BIN" 2>&1 || true
    fi
else
    echo "!!! Store path missing after realise"
    kill $PROXY_PID 2>/dev/null; exit 1
fi

echo "=== Test PASSED ==="
kill $PROXY_PID 2>/dev/null
CONTAINER_SCRIPT
chmod +x "$PROJECT_DIR/test/run-in-container.sh"

CONTAINER_ENGINE="podman"
if ! command -v podman &>/dev/null; then
    if command -v docker &>/dev/null; then
        CONTAINER_ENGINE="docker"
    else
        echo "!!! Neither podman nor docker was found in PATH"
        exit 1
    fi
fi

echo ">>> Running test in $CONTAINER_ENGINE container..."
# Pass GH token for GHCR access (package may be private)
GH_TOKEN_FOR_CONTAINER=$(gh auth token 2>/dev/null || echo "")
$CONTAINER_ENGINE run --rm \
    -v "$PROJECT_DIR:/workspace:ro" \
    -v "$PROJECT_DIR/test/run-in-container.sh:/run-test.sh:ro" \
    -e "NIX_CONFIG=experimental-features = nix-command flakes" \
    -e "GITHUB_TOKEN=${GH_TOKEN_FOR_CONTAINER}" \
    docker.io/nixos/nix:latest \
    bash /run-test.sh "$REPO" "$STORE_HASH"

echo "=== All tests passed ==="
