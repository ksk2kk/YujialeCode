#!/usr/bin/env bash
# 使用私有 deploy.env 启动 NapCat；仓库不保存 QQ 号、token 或机器绝对路径。
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

config_file=""
follow_logs=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --config)
            [[ $# -ge 2 ]] || { printf '%s\n' '--config 缺少文件路径' >&2; exit 2; }
            config_file="$2"
            shift 2
            ;;
        --no-follow)
            follow_logs=0
            shift
            ;;
        -h|--help)
            printf '用法: %s [--config FILE] [--no-follow]\n' "$0"
            exit 0
            ;;
        *)
            printf '未知参数: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done
yjlcoder_load_config "$config_file"
yjlcoder_require_value NAPCAT_WEBUI_TOKEN

if [[ -n "${NAPCAT_COMPOSE_FILE:-}" ]]; then
    compose_file="$(yjlcoder_resolve_config_path "$NAPCAT_COMPOSE_FILE")"
else
    compose_file="$SCRIPT_DIR/compose.yml"
fi
[[ -r "$compose_file" ]] || { printf 'Compose 文件不可读: %s\n' "$compose_file" >&2; exit 2; }

export NAPCAT_UID="${NAPCAT_UID:-$(id -u)}"
export NAPCAT_GID="${NAPCAT_GID:-$(id -g)}"
export NAPCAT_CONFIG_DIR="$(yjlcoder_resolve_config_path "${NAPCAT_CONFIG_DIR:-$SCRIPT_DIR/config}")"
export NAPCAT_QQ_DATA_DIR="$(yjlcoder_resolve_config_path "${NAPCAT_QQ_DATA_DIR:-$SCRIPT_DIR/.config}")"
export NAPCAT_LOG_DIR="$(yjlcoder_resolve_config_path "${NAPCAT_LOG_DIR:-$SCRIPT_DIR/logs}")"
export COMPOSE_PROJECT_NAME="${NAPCAT_COMPOSE_PROJECT_NAME:-napcat}"

mkdir -p "$NAPCAT_CONFIG_DIR" "$NAPCAT_QQ_DATA_DIR" "$NAPCAT_LOG_DIR"
yjlcoder_docker compose -f "$compose_file" up -d
if [[ "$follow_logs" == "1" ]]; then
    yjlcoder_docker compose -f "$compose_file" logs -f --tail="${NAPCAT_LOG_TAIL:-50}"
fi
