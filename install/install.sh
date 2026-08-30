#!/usr/bin/env bash
set -euo pipefail

# 统一安装 nixcache 工具链 (nixcache-builder & nixcache-proxy)
# 支持通过二进制安装 (binary) 或从源码安装 (source)
# 默认通过二进制安装，若已存在则不重复安装 (支持通过 FORCE=true 强制覆盖安装)

verify_nix() {
    if ! command -v nix &> /dev/null; then
        echo "::error::Nix is not installed. Please use DeterminateSystems/nix-installer-action before calling this action."
        exit 1
    fi
}

install_nixcache() {
    verify_nix

    local force="${FORCE:-${INPUT_FORCE:-false}}"
    local source_type="${SOURCE:-${INPUT_SOURCE:-binary}}"
    local version="${VERSION:-${INPUT_VERSION:-}}"
    local script_dir
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    local project_dir
    project_dir="$(dirname "$script_dir")"

    # 1. 检查已存在安装 (若未启用 force 且工具已在 PATH 中)
    if [[ "$force" != "true" ]]; then
        if command -v nixcache-builder &> /dev/null && command -v nixcache-proxy &> /dev/null; then
            local existing_builder
            existing_builder=$(command -v nixcache-builder)
            local existing_dir
            existing_dir=$(dirname "$existing_builder")
            echo ">>> nixcache tools (nixcache-builder & nixcache-proxy) are already installed at $existing_dir."
            echo ">>> Skipping installation. (Set force: true or FORCE=true to force reinstall)"
            if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
                echo "installed=false" >> "$GITHUB_OUTPUT"
                echo "bin-path=$existing_dir" >> "$GITHUB_OUTPUT"
            fi
            return 0
        fi
    fi

    # 2. 确定包名 (binary 默认使用 cache-builder-bin，source 使用 cache-builder)
    local pkg="cache-builder-bin"
    if [[ "$source_type" == "source" || "$source_type" == "src" ]]; then
        pkg="cache-builder"
        echo ">>> Mode: Install from source ($pkg)"
    else
        echo ">>> Mode: Install from pre-compiled binary ($pkg)"
    fi

    # 3. 确定版本 (显式参数 > .nixcache-version > 本地源码/Flake > GITHUB_ACTION_REF > main)
    local ref="$version"
    if [[ -z "$ref" && -f .nixcache-version ]]; then
        ref=$(cat .nixcache-version | tr -d '[:space:]')
        if [[ -n "$ref" ]]; then
            echo ">>> Found .nixcache-version file, using version: $ref"
        fi
    fi

    local target=""
    if [[ -n "$ref" ]]; then
        target="github:shaogme/nixcache-oci/$ref#$pkg"
    elif [[ -f "$project_dir/flake.nix" && -f "$project_dir/nix/binary.nix" ]]; then
        target="$project_dir#$pkg"
    elif [[ -n "${GITHUB_WORKSPACE:-}" && -f "$GITHUB_WORKSPACE/flake.nix" && -f "$GITHUB_WORKSPACE/nix/binary.nix" ]]; then
        target="$GITHUB_WORKSPACE#$pkg"
    else
        local fallback_ref="${GITHUB_ACTION_REF:-main}"
        if [[ -z "$fallback_ref" ]]; then
            fallback_ref="main"
        fi
        target="github:shaogme/nixcache-oci/$fallback_ref#$pkg"
    fi

    echo ">>> Building nixcache target: $target"
    local out_dir="${RUNNER_TEMP:-/tmp}/nixcache-bin-install"
    mkdir -p "$out_dir"
    nix build --accept-flake-config "$target" --out-link "$out_dir/result"

    local bin_dir="$out_dir/result/bin"
    if [[ ! -d "$bin_dir" ]]; then
        echo "::error::Build finished but binary directory $bin_dir was not found."
        exit 1
    fi

    # 4. 将安装路径写入 GITHUB_PATH 并注入当前进程 PATH
    if [[ -n "${GITHUB_PATH:-}" ]]; then
        echo "$bin_dir" >> "$GITHUB_PATH"
    fi
    export PATH="$bin_dir:$PATH"

    if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
        echo "installed=true" >> "$GITHUB_OUTPUT"
        echo "bin-path=$bin_dir" >> "$GITHUB_OUTPUT"
    fi

    echo ">>> Successfully installed nixcache tools:"
    echo "    - nixcache-builder: $bin_dir/nixcache-builder"
    echo "    - nixcache-proxy:   $bin_dir/nixcache-proxy"
}

install_nixcache "$@"
