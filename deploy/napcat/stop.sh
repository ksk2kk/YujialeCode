#!/usr/bin/env bash
# 停止 NapCat 容器；配置和日志目录由 deploy.env 决定并保留。
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

config_file=""
if [[ "${1:-}" == "--config" ]]; then
    [[ $# -ge 2 ]] || { printf '%s\n' '--config 缺少文件路径' >&2; exit 2; }
    config_file="$2"
elif [[ $# -gt 0 ]]; then
    printf '未知参数: %s\n' "$1" >&2
    exit 2
fi
yjlcoder_load_config "$config_file"

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

yjlcoder_docker compose -f "$compose_file" down
