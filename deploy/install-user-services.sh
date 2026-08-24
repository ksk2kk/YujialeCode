#!/usr/bin/env bash
# 把无用户名、无仓库路径依赖的运行脚本安装为 systemd 用户服务。
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
config_source=""
enable_now=0

usage() {
    printf '用法: %s [--config FILE] [--enable]\n' "$0"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --config)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            config_source="$2"
            shift 2
            ;;
        --enable)
            enable_now=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf '未知参数: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/yjlcoder"
config_target="$config_dir/deploy.env"
runtime_dir="$HOME/.local/share/yjlcoder/deploy"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

install -d -m 700 "$config_dir"
install -d -m 755 "$runtime_dir" "$unit_dir"
install -m 755 "$SCRIPT_DIR/common.sh" "$runtime_dir/common.sh"
install -m 755 "$SCRIPT_DIR/yjlcoder-qq.sh" "$runtime_dir/yjlcoder-qq.sh"
install -m 755 "$SCRIPT_DIR/yjlcoder-qr-watcher.sh" "$runtime_dir/yjlcoder-qr-watcher.sh"
install -m 644 "$SCRIPT_DIR/yjlcoder-qq.service" "$unit_dir/yjlcoder-qq.service"
install -m 644 "$SCRIPT_DIR/yjlcoder-qr-watcher.service" "$unit_dir/yjlcoder-qr-watcher.service"

if [[ -n "$config_source" ]]; then
    [[ -r "$config_source" ]] || { printf '配置文件不可读: %s\n' "$config_source" >&2; exit 2; }
    source_dir="$(cd -- "$(dirname -- "$config_source")" && pwd -P)"
    source_path="$source_dir/$(basename -- "$config_source")"
    if [[ "$source_path" != "$config_target" ]]; then
        install -m 600 "$source_path" "$config_target"
    else
        chmod 600 "$config_target"
    fi
elif [[ ! -e "$config_target" ]]; then
    install -m 600 "$SCRIPT_DIR/deploy.env.example" "$config_target"
    printf '已生成配置模板: %s\n请先填写真实值。\n' "$config_target"
fi

systemctl --user daemon-reload
if [[ "$enable_now" == "1" ]]; then
    if grep -q 'replace-with-a-random-token' "$config_target"; then
        printf '拒绝启动：请先修改 %s 中的示例 token。\n' "$config_target" >&2
        exit 2
    fi
    systemctl --user enable --now yjlcoder-qq.service yjlcoder-qr-watcher.service
fi

printf '服务文件已安装。配置: %s\n' "$config_target"
printf '开机免登录运行可执行: sudo loginctl enable-linger "$USER"\n'
