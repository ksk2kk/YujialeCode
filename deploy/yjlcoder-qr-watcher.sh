#!/bin/bash
# YJLcoder QQ 扫码弹窗助手（systemd 用户服务）
#
# 作用：NapCat 未登录（二维码循环）时，自动在浏览器打开 NapCat WebUI 扫码页，
# 用户用手机 QQ 扫码即可登录；登录成功后弹通知并退出。
#
# 判定信号（2026-08-13 二改，弃用 docker logs 与文件存在性）：
#   - v1 用 docker logs grep "请扫描...|ErrCode"：登录后 ErrCode 可能持续出现在
#     无关日志行，导致"已登录"永不成立，服务挂死（生产实测 34 分钟不退出）。
#   - v2 用 docker cp 成功/失败（文件存在性）：登录成功后 NapCat 并不删除
#     qrcode.png，文件残留导致已登录状态误判为未登录、误弹浏览器（生产实测）。
#   - v3（本版）用文件内容哈希：未登录时二维码每 ~2 分钟刷新一次（内容变化）；
#     登录成功后内容冻结。连续 10 轮（200s，> 刷新周期 120s）无变化 = 已登录。
#     只有「内容变化」能区分扫码循环与已登录，静态检查任何单一时刻都无法区分。

# 固定部署参数：脚本整个进程期间不变。WEBUI_URL 是浏览器入口，CONTAINER 是 docker 名。
WEBUI_URL="http://127.0.0.1:6099/webui?token=0ef70d38-c0ab-41e5-b209-300f8bba8ca3"
CONTAINER="napcat"
# 派生路径：QR_FILE 永远位于 QR_DIR 下，保存从容器复制出的当前二维码快照。
QR_DIR="$HOME/.yjlcoder/qq-qr"
QR_FILE="$QR_DIR/qrcode.png"

# 桌面环境变量（systemd 用户服务不继承，从登录会话获取）
export DISPLAY="${DISPLAY:-:0}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"

log() { echo "[qq-qr] $(date +%H:%M:%S) $*"; }
notify() { notify-send -a yjlcoder "$1" "$2" 2>/dev/null || true; }

# 状态机变量只活到脚本退出：opened 记录本轮是否已经弹过页面，避免每 20 秒重复弹窗。
opened=0
# unchanged_rounds 是二维码内容连续不变的轮数；内容变化时清零。
unchanged_rounds=0
# prev_hash 是上一轮 SHA-256；空字符串表示还没有建立比较基线。
prev_hash=""

# 等待容器存在
for _ in $(seq 1 30); do
    if sudo docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$CONTAINER"; then
        break
    fi
    sleep 10
done

mkdir -p "$QR_DIR"
log "开始监听 $CONTAINER 登录状态（未登录时自动打开浏览器）"

while true; do
    sleep 20

    if sudo docker cp "$CONTAINER:/app/napcat/cache/qrcode.png" "$QR_FILE" 2>/dev/null; then
        # cur_hash 只代表本轮文件内容；循环尾部赋给 prev_hash，成为下一轮基线。
        cur_hash=$(sha256sum "$QR_FILE" 2>/dev/null | awk '{print $1}')
        if [ -n "$prev_hash" ] && [ "$cur_hash" = "$prev_hash" ]; then
            # 二维码内容与上一轮相同
            unchanged_rounds=$((unchanged_rounds + 1))
            if [ "$opened" = "1" ] && [ "$unchanged_rounds" -ge 10 ]; then
                log "登录成功（二维码内容连续 10 轮未变），退出"
                notify "QQ 登录成功" "机器人已上线，可关闭扫码页面"
                exit 0
            elif [ "$opened" = "0" ] && [ "$unchanged_rounds" -ge 10 ]; then
                # 从未出现过扫码循环：开机时登录态已有效，无需扫码，静默退出
                log "未检测到扫码循环（登录态有效），静默退出"
                exit 0
            fi
        else
            # 内容变化 = 二维码刷新中 = 未登录，需要扫码（首轮 prev_hash 为空只记录基线，不判定）
            if [ -n "$prev_hash" ]; then
                unchanged_rounds=0
                if [ "$opened" = "0" ]; then
                    log "检测到二维码循环，打开浏览器扫码页"
                    xdg-open "$WEBUI_URL" >/dev/null 2>&1 || notify "QQ 需要扫码" "浏览器打不开，请手动打开: $WEBUI_URL"
                    notify "QQ 需要扫码登录" "手机 QQ 扫码登录（二维码在浏览器里，或 $QR_DIR/qrcode.png）"
                    opened=1
                fi
            fi
        fi
        prev_hash="$cur_hash"
    else
        # 容器内无二维码文件（cp 失败）
        if [ "$opened" = "1" ]; then
            # 曾经有二维码、现在文件消失 = 登录成功
            log "登录成功（二维码文件已消失），退出"
            notify "QQ 登录成功" "机器人已上线，可关闭扫码页面"
            exit 0
        fi
        # 从未见过二维码：容器刚启动尚未生成，继续等待
    fi
done
