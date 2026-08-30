#!/usr/bin/env bash
# test-install-action.sh — Test install action and install script functionality
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "=== Testing NixCache Install Action & Script ==="

TMP_DIR=$(mktemp -d /tmp/nixcache-install-test-XXXXXX)
cleanup() {
    rm -rf "$TMP_DIR"
    rm -rf /homeless-shelter 2>/dev/null || true
}
trap cleanup EXIT
rm -rf /homeless-shelter 2>/dev/null || true

# 1. 测试默认安装 (binary)
echo ">>> Test 1: Default Binary Installation..."
unset NIX_CONFIG || true
export GITHUB_WORKSPACE="$PROJECT_DIR"
export RUNNER_TEMP="$TMP_DIR/run1"
export GITHUB_ENV="$TMP_DIR/github_env"
export GITHUB_PATH="$TMP_DIR/github_path"
export GITHUB_OUTPUT="$TMP_DIR/github_output"
touch "$GITHUB_ENV" "$GITHUB_PATH" "$GITHUB_OUTPUT"

# 彻底清理 PATH 中包含 nixcache 工具的目录以模拟全新环境
CLEAN_PATH=""
IFS=':' read -ra ADDR <<< "$PATH"
for dir in "${ADDR[@]}"; do
    if [[ -z "$dir" ]]; then continue; fi
    if [[ -f "$dir/nixcache-builder" || -f "$dir/nixcache-proxy" || "$dir" == *nixcache* ]]; then
        continue
    fi
    if [[ -z "$CLEAN_PATH" ]]; then
        CLEAN_PATH="$dir"
    else
        CLEAN_PATH="$CLEAN_PATH:$dir"
    fi
done
export PATH="$CLEAN_PATH"
hash -r 2>/dev/null || true

chmod +x "$PROJECT_DIR/install/install.sh"
"$PROJECT_DIR/install/install.sh"

if ! grep -q "installed=true" "$GITHUB_OUTPUT"; then
    echo "!!! Test 1 Failed: Expected installed=true in GITHUB_OUTPUT"
    exit 1
fi

INSTALLED_BIN_DIR=$(grep "bin-path=" "$GITHUB_OUTPUT" | cut -d'=' -f2)
if [[ ! -x "$INSTALLED_BIN_DIR/nixcache-builder" || ! -x "$INSTALLED_BIN_DIR/nixcache-proxy" ]]; then
    echo "!!! Test 1 Failed: Installed binaries not found or not executable"
    exit 1
fi
echo ">>> Test 1 Passed."

# 2. 测试已存在跳过安装 (若在 PATH 中且未设置 force)
echo ">>> Test 2: Skip when already installed..."
: > "$GITHUB_OUTPUT"
PATH="$INSTALLED_BIN_DIR:$CLEAN_PATH" "$PROJECT_DIR/install/install.sh"

if ! grep -q "installed=false" "$GITHUB_OUTPUT"; then
    echo "!!! Test 2 Failed: Expected installed=false when already present"
    exit 1
fi
echo ">>> Test 2 Passed."

# 3. 测试强制覆盖安装 (FORCE=true)
echo ">>> Test 3: Force overwrite installation..."
: > "$GITHUB_OUTPUT"
FORCE=true PATH="$INSTALLED_BIN_DIR:$CLEAN_PATH" "$PROJECT_DIR/install/install.sh"

if ! grep -q "installed=true" "$GITHUB_OUTPUT"; then
    echo "!!! Test 3 Failed: Expected installed=true when FORCE=true"
    exit 1
fi
echo ">>> Test 3 Passed."

# 4. 测试从源码安装 (SOURCE=source)
echo ">>> Test 4: Source installation..."
rm -rf /homeless-shelter 2>/dev/null || true
export RUNNER_TEMP="$TMP_DIR/run4"
: > "$GITHUB_OUTPUT"
SOURCE=source FORCE=true PATH="$CLEAN_PATH" "$PROJECT_DIR/install/install.sh"

if ! grep -q "installed=true" "$GITHUB_OUTPUT"; then
    echo "!!! Test 4 Failed: Expected installed=true for source installation"
    exit 1
fi
SRC_BIN_DIR=$(grep "bin-path=" "$GITHUB_OUTPUT" | cut -d'=' -f2)
if [[ ! -x "$SRC_BIN_DIR/nixcache-builder" || ! -x "$SRC_BIN_DIR/nixcache-proxy" ]]; then
    echo "!!! Test 4 Failed: Source installed binaries not found or not executable"
    exit 1
fi
echo ">>> Test 4 Passed."

echo "=== ALL INSTALL ACTION TESTS PASSED SUCCESSFULLY ==="
