#!/usr/bin/env bash
# systemd 使用的 QQ 无界面启动器：读取本机配置并动态定位 yjlcoder。
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

config_file=""
if [[ "${1:-}" == "--config" ]]; then
    [[ $# -ge 2 ]] || { printf '%s\n' '--config 缺少文件路径' >&2; exit 2; }
    config_file="$2"
elif [[ $# -gt 0 ]]; then
    printf '未知参数: %s\n' "$1" >&2
    exit 2
fi
yjlcoder_load_config "$config_file"

binary="${YJLCODER_BIN:-}"
if [[ -n "$binary" && "$binary" == */* ]]; then
    binary="$(yjlcoder_resolve_config_path "$binary")"
elif [[ -n "$binary" ]]; then
    binary="$(command -v "$binary" 2>/dev/null || true)"
else
    binary="$(command -v yjlcoder 2>/dev/null || true)"
fi
if [[ -z "$binary" && -x "$HOME/.cargo/bin/yjlcoder" ]]; then
    binary="$HOME/.cargo/bin/yjlcoder"
fi
if [[ -z "$binary" || ! -x "$binary" ]]; then
    printf '找不到可执行的 yjlcoder；请在 %s 设置 YJLCODER_BIN。\n' \
        "$YJLCODER_DEPLOY_CONFIG" >&2
    exit 2
fi

exec "$binary" --qq-only
