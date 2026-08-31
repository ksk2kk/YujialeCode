#!/usr/bin/env bash
# 停止本地 SearXNG 容器；web_search 会自动回到免费引擎池（Bing/DDG/Wikipedia）。
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

default_config="$(yjlcoder_default_config)"
if [[ -n "$config_file" || -r "$default_config" ]]; then
    yjlcoder_load_config "$config_file"
fi

if [[ -n "${SEARXNG_COMPOSE_FILE:-}" ]]; then
    compose_file="$(yjlcoder_resolve_config_path "$SEARXNG_COMPOSE_FILE")"
else
    compose_file="$SCRIPT_DIR/docker-compose.yml"
fi
[[ -r "$compose_file" ]] || { printf 'Compose 文件不可读: %s\n' "$compose_file" >&2; exit 2; }

export SEARXNG_PORT="${SEARXNG_PORT:-8888}"
export COMPOSE_PROJECT_NAME="${SEARXNG_COMPOSE_PROJECT_NAME:-yjlcoder-searxng}"

yjlcoder_docker compose -f "$compose_file" down
printf 'web_search 已自动回到免费引擎池（Bing/DDG/Wikipedia），无需其它改动。\n'
