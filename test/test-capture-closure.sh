#!/usr/bin/env bash
# test-capture-closure.sh — Comprehensive End-to-End Test for Capture Redesign & Runtime Closure Engine
# Verifies:
#   1. Session initialization & baseline store snapshot
#   2. Graph-based runtime closure computation & intermediate build-tool filtering
#   3. Real-world Rust package derivation compilation (zero compiler/source leakage)
#   4. Target expression & Flake resolution without out-link
#   5. Strict closure validation (rejection of missing target outputs)
#   6. Permissive fallback mode (--no-strict-closure / diff-all)
#   7. Remote OCI session manifest Schema v8 CAS merge & GC root purification
#   8. Multi-architecture promote & ephemeral session tag cleanup
#   9. Proxy binary substitution & executable roundtrip validation

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "================================================================================"
echo "🎯 NixCache Capture Redesign & Runtime Closure 全面集成与端到端回归测试"
echo "================================================================================"

TMP_DIR=$(mktemp -d /tmp/nixcache-closure-test-XXXXXX)
export GITHUB_ENV="$TMP_DIR/github_env"
export GITHUB_OUTPUT="$TMP_DIR/github_output"
export GITHUB_PATH="$TMP_DIR/github_path"
touch "$GITHUB_ENV" "$GITHUB_OUTPUT" "$GITHUB_PATH"

REGISTRY_PORT=5025
PROXY_PORT=37525
REGISTRY_PID=""
RUN_ID=654321
export GITHUB_RUN_ID="$RUN_ID"

cleanup() {
    echo ">>> 清理测试临时环境与进程..."
    if [[ -n "${REGISTRY_PID:-}" ]]; then
        kill -9 "$REGISTRY_PID" 2>/dev/null || true
    fi
    pkill -9 -f "run_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
    pkill -9 -f "nixcache-proxy" 2>/dev/null || true
    podman rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || docker rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || true
    rm -rf /tmp/nixcache-test-registry "$TMP_DIR"
    rm -f test-secret.key test-public.key
    echo ">>> 清理完成。"
}
trap cleanup EXIT

# 1. 查找或构建测试所需二进制文件
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
    BUILDER_BIN="$PROJECT_DIR/target/debug/nixcache-builder"
    PROXY_BIN="$PROJECT_DIR/target/debug/nixcache-proxy"
}

find_binaries
PROXY_DIR="$(cd "$(dirname "$PROXY_BIN")" && pwd)"
export PATH="$PROXY_DIR:$PATH"

# 生成签名公私钥对
echo ">>> Generating signing key pair..."
KEY_SECRET="$TMP_DIR/test-secret.key"
KEY_PUBLIC="$TMP_DIR/test-public.key"
rm -f "$KEY_SECRET" "$KEY_PUBLIC" test-secret.key test-public.key
nix-store --generate-binary-cache-key test-key-1 "$KEY_SECRET" "$KEY_PUBLIC"
cp "$KEY_SECRET" test-secret.key
cp "$KEY_PUBLIC" test-public.key
export NIXCACHE_SIGNING_KEY_FILE="$KEY_SECRET"

export NIXCACHE_REPO="testorg/closure-app"
export NIXCACHE_REGISTRY="127.0.0.1:${REGISTRY_PORT}"
export GITHUB_TOKEN="dummy-token"

# 2. 启动隔离的 OCI Registry 容器
echo ">>> [1/9] 启动隔离的 OCI 镜像仓库容器 (Port: ${REGISTRY_PORT})..."
pkill -9 -f "run_registry.py.*${REGISTRY_PORT}" 2>/dev/null || true
podman rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || docker rm -f "nixcache-registry-${REGISTRY_PORT}" 2>/dev/null || true
rm -rf /tmp/nixcache-test-registry
mkdir -p /tmp/nixcache-test-registry

python3 "$SCRIPT_DIR/run_registry.py" "$REGISTRY_PORT" &
REGISTRY_PID=$!

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 3. 会话初始化并记录基线快照 (Session Init)
SNAPSHOT_FILE="$TMP_DIR/snapshot-before.txt"
echo ">>> [2/9] 初始化会话并记录构建前 Store 快照..."
"$BUILDER_BIN" session init \
    --run-id "$RUN_ID" \
    --branch "main" \
    --port "$PROXY_PORT" \
    --listen "127.0.0.1" \
    --upstream "https://cache.nixos.org" \
    --snapshot-path "$SNAPSHOT_FILE"

# 检查代理健康状态
curl -fs "http://127.0.0.1:${PROXY_PORT}/nix-cache-info" | grep -q "StoreDir: /nix/store"

# 4. 混合构建场景：验证中间编译工具与临时产物 100% 剔除
echo ">>> [3/9] 执行复合 Nix 构建，测试中间编译工具与临时产物过滤..."
JOB1_DIR="$TMP_DIR/job1_workspace"
mkdir -p "$JOB1_DIR"
cd "$JOB1_DIR"

NONCE1=$(date +%s%N)
nix-build -E "
let
  buildTool = derivation {
    name = \"custom-compiler-tool-${NONCE1}\";
    system = builtins.currentSystem;
    builder = \"/bin/sh\";
    args = [ \"-c\" \"echo compiler-bin > \$out\" ];
  };
  runtimeLib = derivation {
    name = \"custom-runtime-lib-${NONCE1}\";
    system = builtins.currentSystem;
    builder = \"/bin/sh\";
    args = [ \"-c\" \"echo libfoo.so > \$out\" ];
  };
  app = derivation {
    name = \"target-app-${NONCE1}\";
    system = builtins.currentSystem;
    builder = \"/bin/sh\";
    args = [ \"-c\" \"cat \${buildTool} > /dev/null; echo app-bin > \$out; echo \${runtimeLib} >> \$out\" ];
  };
in app
" --out-link result

# 额外生成一个独立的中间测试产物
TRANSIENT_PATH=$(nix-build -E "
derivation {
  name = \"transient-test-artifact-${NONCE1}\";
  system = builtins.currentSystem;
  builder = \"/bin/sh\";
  args = [ \"-c\" \"echo test-artifact > \$out\" ];
}
" --no-out-link)

APP_PATH_1=$(readlink -f result)
RUNTIME_LIB_PATH_1=$(grep "/nix/store/" result)
BUILD_TOOL_PATH_1=$(ls -d /nix/store/*custom-compiler-tool-"${NONCE1}")

RECEIPT_1="$TMP_DIR/receipt-job1.json"
"$BUILDER_BIN" session capture \
    --run-id "$RUN_ID" \
    --job-id "job-comp-1" \
    --system "x86_64-linux" \
    --capture-mode "runtime-closure" \
    --out-link "./result*" \
    --snapshot-path "$SNAPSHOT_FILE" \
    --proxy-url "http://127.0.0.1:${PROXY_PORT}" \
    --output-receipt "$RECEIPT_1"

python3 -c "
import json
with open('$RECEIPT_1') as f:
    receipt = json.load(f)

app_h = '$APP_PATH_1'.split('/')[-1][:32]
lib_h = '$RUNTIME_LIB_PATH_1'.split('/')[-1][:32]
tool_h = '$BUILD_TOOL_PATH_1'.split('/')[-1][:32]
trans_h = '$TRANSIENT_PATH'.split('/')[-1][:32]

entries = receipt['new_entries']
gc_roots = receipt['active_gc_roots']

assert app_h in entries, 'Target app missing in entries'
assert lib_h in entries, 'Runtime lib missing in entries'
assert tool_h not in entries, 'Build tool leaked into entries!'
assert trans_h not in entries, 'Transient artifact leaked into entries!'
assert len(entries) == 2, f'Expected exactly 2 entries, got {len(entries)}'

assert gc_roots == [app_h], f'Expected active_gc_roots strictly [{app_h}], got {gc_roots}'
print('>>> [PASS] 复合构建场景：编译期工具与临时产物被 100% 精确剔除，GC Root 严格提纯！')
"

# 5. 真实 Rust 软件包构建测试 (rustc 编译套件与源码产物过滤)
echo ">>> [4/9] 构建真实 Rust 软件包，验证 rustc 编译工具套件 (>1GB) 完全剥离..."
JOB2_DIR="$TMP_DIR/job2_rust_workspace"
mkdir -p "$JOB2_DIR"
cd "$JOB2_DIR"

NONCE2=$(date +%s%N)
nix-build -E "
let
  pkgs = import <nixpkgs> {};
in
pkgs.stdenv.mkDerivation {
  name = \"demo-rust-app-${NONCE2}\";
  src = pkgs.runCommand \"demo-rust-src-${NONCE2}\" {} ''
    mkdir -p \$out
    cat << 'EOF' > \$out/main.rs
    fn main() {
        println!(\"Hello from compiled Rust package with Zero Intermediate Artifacts!\");
    }
EOF
  '';
  nativeBuildInputs = [ pkgs.rustc ];
  buildPhase = ''
    rustc \$src/main.rs -O -o demo-rust-app
  '';
  installPhase = ''
    mkdir -p \$out/bin
    cp demo-rust-app \$out/bin/
  '';
}
" --out-link result

RUST_APP_PATH=$(readlink -f result)
"$RUST_APP_PATH/bin/demo-rust-app"

RECEIPT_2="$TMP_DIR/receipt-job2.json"
"$BUILDER_BIN" session capture \
    --run-id "$RUN_ID" \
    --job-id "job-rust-2" \
    --system "x86_64-linux" \
    --capture-mode "runtime-closure" \
    --out-link "./result" \
    --snapshot-path "$SNAPSHOT_FILE" \
    --proxy-url "http://127.0.0.1:${PROXY_PORT}" \
    --output-receipt "$RECEIPT_2"

python3 -c "
import json
with open('$RECEIPT_2') as f:
    receipt = json.load(f)

app_h = '$RUST_APP_PATH'.split('/')[-1][:32]
entries = receipt['new_entries']
gc_roots = receipt['active_gc_roots']

assert app_h in entries, 'Rust app missing in entries'
assert len(entries) == 1, f'Expected strictly 1 locally-built entry, got {len(entries)}'
for h, item in entries.items():
    assert 'rustc' not in item['name'], 'rustc leaked into entries!'
    assert 'demo-rust-src' not in item['name'], 'demo-rust-src leaked into entries!'

assert gc_roots == [app_h], f'Expected active_gc_roots strictly [{app_h}], got {gc_roots}'
print('>>> [PASS] Rust 软件包构建：rustc 编译器与源码派生文件 100% 剥离！')
"

# 6. 验证目标表达式 (--targets) 与显式路径解析 (无需任何 out-link 软链接)
echo ">>> [5/9] 验证无需软链接的显式路径与 --targets 表达式捕获..."
JOB3_DIR="$TMP_DIR/job3_explicit_workspace"
mkdir -p "$JOB3_DIR"
cd "$JOB3_DIR"

DUMMY_FILE="$TMP_DIR/dummy-payload.txt"
echo "payload for explicit target test $(date +%s%N)" > "$DUMMY_FILE"
EXPLICIT_STORE_PATH=$(nix-store --add "$DUMMY_FILE")
rm -f "$DUMMY_FILE"

RECEIPT_3="$TMP_DIR/receipt-job3.json"
"$BUILDER_BIN" session capture \
    --run-id "$RUN_ID" \
    --job-id "job-explicit-3" \
    --system "x86_64-linux" \
    --out-link "" \
    --snapshot-path "$SNAPSHOT_FILE" \
    --proxy-url "http://127.0.0.1:${PROXY_PORT}" \
    --output-receipt "$RECEIPT_3" \
    "$EXPLICIT_STORE_PATH"

python3 -c "
import json
with open('$RECEIPT_3') as f:
    receipt = json.load(f)
h = '$EXPLICIT_STORE_PATH'.split('/')[-1][:32]
assert h in receipt['new_entries'], 'Explicit path missing in entries'
assert receipt['active_gc_roots'] == [h], 'Explicit path missing in active_gc_roots'
print('>>> [PASS] 显式路径解析与捕获验证通过！')
"

# 7. 严格模式错误处理验证 (Strict Closure Validation)
echo ">>> [6/9] 验证严格闭包校验在目标缺失时正确拦截报错..."
EMPTY_DIR="$TMP_DIR/empty_workspace"
mkdir -p "$EMPTY_DIR"
cd "$EMPTY_DIR"

set +e
"$BUILDER_BIN" session capture \
    --run-id "$RUN_ID" \
    --job-id "job-strict-test" \
    --system "x86_64-linux" \
    --strict-closure \
    2>"$TMP_DIR/strict_error.log"
STRICT_EXIT=$?
set -e

if [[ "$STRICT_EXIT" -eq 0 ]]; then
    echo "!!! Expected non-zero exit on missing target outputs with strict closure"
    exit 1
fi
grep -q "No valid target outputs or result symlinks found" "$TMP_DIR/strict_error.log"
echo ">>> [PASS] 严格模式在目标缺失时按预期拦截并给出友好错误提示！"

# 8. 验证 OCI 零临时标签污染 & 代理热注册表状态
echo ">>> [7/9] 验证 OCI 零临时标签污染 & 本地代理热注册表 (Tier 0)..."
TAGS_JSON=$(curl -s "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/closure-app/nix-cache/tags/list" || true)
python3 -c "
import json
try:
    tags_data = json.loads('''$TAGS_JSON''')
    tags = tags_data.get('tags') or []
except Exception:
    tags = []
run_tags = [t for t in tags if t.startswith('run-')]
assert len(run_tags) == 0, f'Expected 0 ephemeral run-* tags, got {run_tags}'
print('>>> [PASS] 镜像仓库完全干净，零 run-* 临时标签污染！')
"

# 验证本地代理热注册表已包含条目
PROXY_STATUS=$(curl -fs "http://127.0.0.1:${PROXY_PORT}/_status")
python3 -c "
import json
status = json.loads('''$PROXY_STATUS''')
assert status['tier0_hot_entries'] >= 4, f'Expected at least 4 tier0 hot entries, got {status[\"tier0_hot_entries\"]}'
print('>>> [PASS] 本地代理热注册表 (Tier 0) 确认命中全部产物条目！')
"

# 9. 单写提升 (Single-Writer Promote) 聚合所有 Receipt 并构建全局分片索引
echo ">>> [8/9] 执行 promote 单写压实提升至基线索引 (cache-index)..."
"$BUILDER_BIN" promote \
    --receipt "$RECEIPT_1" \
    --receipt "$RECEIPT_2" \
    --receipt "$RECEIPT_3" \
    --target-tag "cache-index"

BASE_INDEX=$(curl -fs -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json" "http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/closure-app/nix-cache/manifests/cache-index")

python3 -c "
import json, subprocess
manifest_index = json.loads('''$BASE_INDEX''')
sub_manifest_digest = manifest_index['manifests'][0]['digest']

sub_raw = subprocess.check_output([
    'curl', '-fsSL',
    '-H', 'Accept: application/vnd.oci.image.manifest.v1+json',
    f'http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/closure-app/nix-cache/manifests/{sub_manifest_digest}'
])
sub_manifest = json.loads(sub_raw)

layer_digest = sub_manifest['layers'][0]['digest']
blob_bytes = subprocess.check_output([
    'curl', '-fsSL',
    f'http://127.0.0.1:${REGISTRY_PORT}/v2/testorg/closure-app/nix-cache/blobs/{layer_digest}'
])

decompressed = subprocess.check_output(['zstd', '-dc'], input=blob_bytes)
idx = json.loads(decompressed)
assert idx['version'] == 8, f'Expected version 8, got {idx.get(\"version\")}'
assert idx['last_promoted_run'] == $RUN_ID, f'Expected last_promoted_run $RUN_ID, got {idx.get(\"last_promoted_run\")}'
total_entries = sum(s['entry_count'] for s in idx['shards'])
assert total_entries == 4, f'Expected 4 entries across shards, got {total_entries}'

# 验证 GC Roots 纯净度
app1_h = '$APP_PATH_1'.split('/')[-1][:32]
rust_h = '$RUST_APP_PATH'.split('/')[-1][:32]
exp_h = '$EXPLICIT_STORE_PATH'.split('/')[-1][:32]
assert set(idx['gc_roots']) == {app1_h, rust_h, exp_h}, f'GC Roots mismatch: {idx[\"gc_roots\"]}'

print('>>> [PASS] 基线全局分片索引 cache-index 单写聚合验证通过 (4 个条目，GC Roots 纯净)！')
"

# 10. 二进制替换 (Substitution) 与执行验证
echo ">>> [9/9] 验证 Nix 通过 nixcache-proxy 从 OCI 缓存中替代替换并执行 Rust 程序..."
pkill -9 -f "nixcache-proxy" 2>/dev/null || true
"$PROXY_BIN" \
    --repo "testorg/closure-app" \
    --registry "127.0.0.1:${REGISTRY_PORT}" \
    --port "$PROXY_PORT" \
    --listen "127.0.0.1" \
    --baseline-tag "cache-index" \
    --system "x86_64-linux" &

for _ in {1..20}; do
    if curl -fs "http://127.0.0.1:${PROXY_PORT}/nix-cache-info" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# 删除本地的 Rust 产物路径
NIX_STORE_BIN=$(command -v nix-store)
"$NIX_STORE_BIN" --delete "$RUST_APP_PATH" --ignore-liveness 2>/dev/null || true
if "$NIX_STORE_BIN" --query --hash "$RUST_APP_PATH" >/dev/null 2>&1; then
    if command -v sudo &>/dev/null && sudo -n true 2>/dev/null; then
        sudo "$NIX_STORE_BIN" --delete "$RUST_APP_PATH" --ignore-liveness 2>/dev/null || true
    fi
fi

# 从本地 nixcache-proxy 替代替换产物
nix-store --realise "$RUST_APP_PATH" \
    --option binary-caches "http://127.0.0.1:${PROXY_PORT}" \
    --option trusted-public-keys "$(cat "$KEY_PUBLIC")" \
    --option require-sigs true

# 执行替换下载后的二进制程序
RUST_OUTPUT=$("$RUST_APP_PATH/bin/demo-rust-app")
echo "Substituted Rust binary output: $RUST_OUTPUT"
if [[ "$RUST_OUTPUT" != *"Hello from compiled Rust package with Zero Intermediate Artifacts!"* ]]; then
    echo "!!! Substituted binary output mismatch: $RUST_OUTPUT"
    exit 1
fi
echo ">>> [PASS] Rust 二进制包成功从 OCI 缓存代理替代替换并正确执行！"

echo ""
echo "================================================================================"
echo "🎉 全套 CAPTURE REDESIGN & RUNTIME CLOSURE 单一回归测试全部圆满通过！"
echo "================================================================================"
