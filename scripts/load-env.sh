#!/usr/bin/env bash

# 统一加载环境变量的脚本
# 用法: source scripts/load-env.sh [example_mode]
# 如果 GITHUB_ENV 变量存在，它会把变量输出到 $GITHUB_ENV 中；否则直接 export。

load_nixcache_env() {
    local example_mode="${1:-}"
    local SCRIPT_DIR
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    local PROJECT_DIR
    PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
    
    load_env_file() {
        local env_file="$1"
        if [[ -f "$env_file" ]]; then
            echo ">>> Loading environment variables from $env_file"
            while IFS= read -r line || [[ -n "$line" ]]; do
                if [[ -z "$line" || "$line" =~ ^# ]]; then
                    continue
                fi
                line=$(echo "$line" | xargs || echo "$line")
                if [[ -z "$line" || "$line" =~ ^# ]]; then
                    continue
                fi
                local cleaned_line="${line#export }"
                if [[ -n "${GITHUB_ENV:-}" ]]; then
                    echo "$cleaned_line" >> "$GITHUB_ENV"
                fi
                export "$cleaned_line"
            done < "$env_file"
        fi
    }
    
    # 1. 加载默认配置
    load_env_file "$PROJECT_DIR/env/default.env"
    
    # 2. 提取 NIXCACHE_EXAMPLE 决定是否加载示例环境
    local local_example="0"
    if [[ -f "$PROJECT_DIR/env/default.env" ]]; then
        local_example=$(grep -v '^#' "$PROJECT_DIR/env/default.env" | grep 'NIXCACHE_EXAMPLE=' | cut -d'=' -f2 | tr -d '[:space:]' || true)
    fi
    
    if [[ "$local_example" == "1" ]]; then
        if [[ -z "$example_mode" ]]; then
            example_mode="flake"
        fi
        if [[ "$example_mode" == "flake" ]]; then
            load_env_file "$PROJECT_DIR/env/flake.example.env"
        elif [[ "$example_mode" == "legacy" ]]; then
            load_env_file "$PROJECT_DIR/env/legacy.example.env"
        fi
    fi
}

load_nixcache_env "$@"
