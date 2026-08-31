#!/usr/bin/env bash
# 一键启动本地免费聚合搜索（SearXNG）：并发聚合 Google/Bing/DDG 等数十家引擎，无任何 key。
# web_search 的 auto 后端会自动探测 127.0.0.1:8888 并纳入聚合池（探测缓存 60 秒）。
# deploy.env 可选：不存在也直接可用，仅用于覆盖端口（SEARXNG_PORT）或镜像等。
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

# SearXNG 不含任何私密值，deploy.env 缺失时直接使用默认值
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

yjlcoder_docker compose -f "$compose_file" up -d

# 等待健康检查通过（首次拉取镜像可能较慢）
printf '等待 SearXNG 就绪'
for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:${SEARXNG_PORT}/healthz" >/dev/null 2>&1; then
        printf ' 就绪\n'
        printf '本地聚合搜索已启动: http://127.0.0.1:%s\n' "$SEARXNG_PORT"
        printf 'web_search 的 auto 后端将自动探测并纳入聚合池（缓存 60 秒，稍候即生效）。\n'
        if [[ "$follow_logs" == "1" ]]; then
            yjlcoder_docker compose -f "$compose_file" logs -f --tail="${SEARXNG_LOG_TAIL:-30}"
        fi
        exit 0
    fi
    printf '.'
    sleep 1
done
printf ' 超时\n' >&2
printf '容器可能仍在启动；稍后用 curl http://127.0.0.1:%s/healthz 确认。\n' "$SEARXNG_PORT" >&2
exit 1
