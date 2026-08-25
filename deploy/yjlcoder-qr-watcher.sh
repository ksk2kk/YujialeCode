#!/usr/bin/env bash
# NapCat 原生 WebUI 登录监听器。二维码始终由前端状态机持有，不复制、不截图、不刷新。
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
WEBUI_TOKEN="${NAPCAT_WEBUI_TOKEN:-}"
WEBUI_SCHEME="${NAPCAT_WEBUI_SCHEME:-http}"
WEBUI_HOST="${NAPCAT_WEBUI_HOST:-127.0.0.1}"
WEBUI_PORT="${NAPCAT_WEBUI_PORT:-6099}"
WEBUI_PATH="${NAPCAT_WEBUI_PATH:-/webui}"
STARTUP_ROUNDS="${YJLCODER_QR_STARTUP_ROUNDS:-30}"
STARTUP_INTERVAL="${YJLCODER_QR_STARTUP_INTERVAL:-10}"
OPEN_BIN="${YJLCODER_OPEN_BIN:-xdg-open}"
NOTIFY_BIN="${YJLCODER_NOTIFY_BIN:-notify-send}"

[[ -n "$WEBUI_TOKEN" ]] || { printf '%s\n' 'NAPCAT_WEBUI_TOKEN 不能为空' >&2; exit 2; }
[[ "$WEBUI_SCHEME" == "http" || "$WEBUI_SCHEME" == "https" ]] || {
    printf '%s\n' 'NAPCAT_WEBUI_SCHEME 只允许 http 或 https' >&2
    exit 2
}
[[ "$WEBUI_PORT" =~ ^[1-9][0-9]{0,4}$ ]] && ((WEBUI_PORT <= 65535)) || {
    printf '%s\n' 'NAPCAT_WEBUI_PORT 必须在 1..=65535' >&2
    exit 2
}

urlencode() {
    local LC_ALL=C input="$1" output="" char hex i
    for ((i = 0; i < ${#input}; i++)); do
        char="${input:i:1}"
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

WEBUI_PATH="/${WEBUI_PATH#/}"
WEBUI_PATH="${WEBUI_PATH%/}/"
WEBUI_URL="${WEBUI_SCHEME}://${WEBUI_HOST}:${WEBUI_PORT}${WEBUI_PATH}?token=$(urlencode "$WEBUI_TOKEN")"

for value_name in STARTUP_ROUNDS STARTUP_INTERVAL; do
    value="${!value_name}"
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        printf '%s 必须是正整数，当前值: %s\n' "$value_name" "$value" >&2
        exit 2
    fi
done

# systemd 用户服务通常已有这些变量；没有时只补可由 uid 推导的通用值。
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"

log() { printf '[qq-qr] %s %s\n' "$(date +%H:%M:%S)" "$*"; }
notify() {
    command -v "$NOTIFY_BIN" >/dev/null 2>&1 || return 0
    "$NOTIFY_BIN" -a yjlcoder "$1" "$2" 2>/dev/null || true
}
open_frontend() {
    command -v "$OPEN_BIN" >/dev/null 2>&1 || return 1
    local attempt
    for ((attempt = 1; attempt <= 30; attempt++)); do
        if (exec 3<>"/dev/tcp/${WEBUI_HOST}/${WEBUI_PORT}") 2>/dev/null; then
            break
        fi
        ((attempt < 30)) || return 1
        sleep 1
    done
    "$OPEN_BIN" "$WEBUI_URL" >/dev/null 2>&1
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

log "开始监听 $CONTAINER 登录状态；将打开 NapCat 原生 WebUI，令牌不会写入日志"

is_logged_line() {
    [[ "$1" == *"登录成功"* || "$1" == *"已登录,无法重复登录"* || "$1" == *"QRCodeLoginSucceed"* || "$1" == *"OneBot11 server started"* ]]
}

is_qr_line() {
    [[ "$1" == *"二维码已保存"* || "$1" == *"二维码已更新"* || "$1" == *"请扫描下面的二维码"* ]]
}

# 只检查最后一个可靠状态事件。二维码文件是否稳定或消失都不能代表登录成功。
recent="$(yjlcoder_docker logs --tail 300 "$CONTAINER" 2>/dev/null || true)"
last_state="$(printf '%s\n' "$recent" | grep -E '登录成功|已登录,无法重复登录|QRCodeLoginSucceed|OneBot11 server started|二维码已保存|二维码已更新|请扫描下面的二维码' | tail -n 1 || true)"
if [[ -n "$last_state" ]] && is_logged_line "$last_state"; then
    log "检测到可靠的登录成功事件，机器人已在线"
    exit 0
fi
if open_frontend; then
    log "NapCat 原生 WebUI 已打开；请在该页面扫码，期间不要刷新页面"
else
    notify "QQ 需要扫码登录" "请打开 NapCat 原生 WebUI 登录页"
    log "无法自动打开浏览器，请检查 YJLCODER_OPEN_BIN 和图形会话环境"
fi

# 一条长期日志流只判断结果，不再读取二维码文件或驱动前端刷新。
while IFS= read -r line; do
    if is_logged_line "$line"; then
        log "检测到 QQ 登录成功，停止二维码监听"
        notify "QQ 登录成功" "机器人已上线"
        exit 0
    fi
    if is_qr_line "$line"; then
        log "NapCat 前端二维码状态已更新；保持当前页面，不重新打开或刷新"
    fi
done < <(yjlcoder_docker logs --since 1s -f "$CONTAINER" 2>/dev/null)

log "NapCat 日志流意外结束"
exit 1
