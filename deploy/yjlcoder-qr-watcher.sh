#!/usr/bin/env bash
# NapCat 扫码监听器。所有机器相关值来自 deploy.env，不保存真实 token、用户名或仓库路径。
set -Eeuo pipefail
umask 077

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

CONTAINER="${NAPCAT_CONTAINER_NAME:-napcat}"
QR_SOURCE="${NAPCAT_QR_SOURCE:-/app/napcat/cache/qrcode.png}"
if [[ -n "${YJLCODER_QR_DIR:-}" ]]; then
    QR_DIR="$(yjlcoder_resolve_config_path "$YJLCODER_QR_DIR")"
else
    QR_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/yjlcoder/qq-qr"
fi
QR_FILE="$QR_DIR/qrcode.png"
POLL_SECONDS="${YJLCODER_QR_POLL_SECONDS:-20}"
STABLE_ROUNDS="${YJLCODER_QR_STABLE_ROUNDS:-10}"
STARTUP_ROUNDS="${YJLCODER_QR_STARTUP_ROUNDS:-30}"
STARTUP_INTERVAL="${YJLCODER_QR_STARTUP_INTERVAL:-10}"
OPEN_BIN="${YJLCODER_OPEN_BIN:-xdg-open}"
NOTIFY_BIN="${YJLCODER_NOTIFY_BIN:-notify-send}"

for value_name in POLL_SECONDS STABLE_ROUNDS STARTUP_ROUNDS STARTUP_INTERVAL; do
    value="${!value_name}"
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        printf '%s 必须是正整数，当前值: %s\n' "$value_name" "$value" >&2
        exit 2
    fi
done

# systemd 用户服务通常已有这些变量；没有时只补可由 uid 推导的通用值。
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"

urlencode() {
    local input="$1" output="" char hex index
    local LC_ALL=C
    for ((index = 0; index < ${#input}; index++)); do
        char="${input:index:1}"
        case "$char" in
            [a-zA-Z0-9.~_-]) output+="$char" ;;
            *)
                printf -v hex '%%%02X' "'$char"
                output+="$hex"
                ;;
        esac
    done
    printf '%s' "$output"
}

if [[ -n "${NAPCAT_WEBUI_URL:-}" ]]; then
    WEBUI_BASE_URL="$NAPCAT_WEBUI_URL"
else
    WEBUI_BASE_URL="${NAPCAT_WEBUI_SCHEME:-http}://${NAPCAT_WEBUI_HOST:-127.0.0.1}:${NAPCAT_WEBUI_PORT:-6099}${NAPCAT_WEBUI_PATH:-/webui}"
fi
WEBUI_URL="$WEBUI_BASE_URL"
if [[ -n "${NAPCAT_WEBUI_TOKEN:-}" ]]; then
    separator='?'
    [[ "$WEBUI_URL" == *\?* ]] && separator='&'
    WEBUI_URL+="${separator}token=$(urlencode "$NAPCAT_WEBUI_TOKEN")"
fi

log() { printf '[qq-qr] %s %s\n' "$(date +%H:%M:%S)" "$*"; }
notify() {
    command -v "$NOTIFY_BIN" >/dev/null 2>&1 || return 0
    "$NOTIFY_BIN" -a yjlcoder "$1" "$2" 2>/dev/null || true
}
open_webui() {
    command -v "$OPEN_BIN" >/dev/null 2>&1 || return 1
    "$OPEN_BIN" "$WEBUI_URL" >/dev/null 2>&1
}
hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        printf '%s\n' '缺少 sha256sum/shasum，无法比较二维码内容' >&2
        return 1
    fi
}
container_running() {
    [[ "$(yjlcoder_docker inspect --format '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)" == "true" ]]
}

for ((attempt = 1; attempt <= STARTUP_ROUNDS; attempt++)); do
    container_running && break
    if ((attempt == STARTUP_ROUNDS)); then
        log "等待容器 $CONTAINER 超时"
        exit 1
    fi
    sleep "$STARTUP_INTERVAL"
done

mkdir -p "$QR_DIR"
log "开始监听 $CONTAINER 登录状态；WebUI 地址和 token 来自 $YJLCODER_DEPLOY_CONFIG"

opened=0
unchanged_rounds=0
prev_hash=""

while true; do
    if yjlcoder_docker cp "$CONTAINER:$QR_SOURCE" "$QR_FILE" 2>/dev/null; then
        cur_hash="$(hash_file "$QR_FILE")"
        if [[ -n "$prev_hash" && "$cur_hash" == "$prev_hash" ]]; then
            unchanged_rounds=$((unchanged_rounds + 1))
            if ((unchanged_rounds >= STABLE_ROUNDS)); then
                if [[ "$opened" == "1" ]]; then
                    log "二维码内容稳定，判定登录成功"
                    notify "QQ 登录成功" "机器人已上线，可关闭扫码页面"
                else
                    log "未检测到扫码循环，登录态仍有效"
                fi
                exit 0
            fi
        else
            if [[ -n "$prev_hash" ]]; then
                unchanged_rounds=0
                if [[ "$opened" == "0" ]]; then
                    log "检测到二维码刷新，打开 NapCat WebUI"
                    open_webui || notify "QQ 需要扫码" "请打开 $WEBUI_BASE_URL；token 保存在部署配置中"
                    notify "QQ 需要扫码登录" "二维码快照位于 $QR_FILE"
                    opened=1
                fi
            fi
        fi
        prev_hash="$cur_hash"
    elif [[ "$opened" == "1" ]]; then
        log "二维码文件消失，判定登录成功"
        notify "QQ 登录成功" "机器人已上线，可关闭扫码页面"
        exit 0
    fi
    sleep "$POLL_SECONDS"
done
