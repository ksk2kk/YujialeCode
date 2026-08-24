#!/usr/bin/env bash
# deploy 下所有脚本共用的配置加载与 Docker 调用层。
# 配置文件是普通 Bash KEY=VALUE 文件；默认位置可用 YJLCODER_DEPLOY_CONFIG 覆盖。

yjlcoder_default_config() {
    printf '%s\n' "${YJLCODER_DEPLOY_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/yjlcoder/deploy.env}"
}

yjlcoder_load_config() {
    local config_file="${1:-$(yjlcoder_default_config)}"
    if [[ ! -r "$config_file" ]]; then
        printf '部署配置不可读: %s\n' "$config_file" >&2
        printf '先复制 deploy/deploy.env.example，或使用 --config FILE。\n' >&2
        return 2
    fi

    local config_dir
    config_dir="$(cd -- "$(dirname -- "$config_file")" && pwd -P)"
    config_file="$config_dir/$(basename -- "$config_file")"

    # 配置由当前用户管理，加载后自动导出给 Docker Compose 和子进程。
    set -a
    # shellcheck disable=SC1090
    source "$config_file"
    set +a
    export YJLCODER_DEPLOY_CONFIG="$config_file"
    export YJLCODER_DEPLOY_CONFIG_DIR="$config_dir"
}

# 配置里的相对路径统一相对于 deploy.env 所在目录，而不是调用脚本时的工作目录。
yjlcoder_resolve_config_path() {
    local path="$1"
    if [[ "$path" == /* ]]; then
        printf '%s\n' "$path"
    else
        printf '%s/%s\n' "$YJLCODER_DEPLOY_CONFIG_DIR" "$path"
    fi
}

yjlcoder_require_value() {
    local name="$1"
    local value="${!name:-}"
    if [[ -z "$value" ]]; then
        printf '部署配置缺少必填项 %s（文件: %s）\n' \
            "$name" "${YJLCODER_DEPLOY_CONFIG:-未指定}" >&2
        return 2
    fi
}

# DOCKER_BIN 可指定 docker 的绝对路径；DOCKER_USE_SUDO 支持 auto/yes/no。
yjlcoder_docker() {
    local docker_bin="${DOCKER_BIN:-docker}"
    local sudo_mode="${DOCKER_USE_SUDO:-auto}"

    case "$sudo_mode" in
        no|NO|No|false|FALSE|False|never|NEVER|Never|0)
            "$docker_bin" "$@"
            ;;
        yes|YES|Yes|true|TRUE|True|always|ALWAYS|Always|1)
            sudo "$docker_bin" "$@"
            ;;
        auto|AUTO|Auto)
            if "$docker_bin" info >/dev/null 2>&1; then
                "$docker_bin" "$@"
            elif command -v sudo >/dev/null 2>&1 && sudo -n "$docker_bin" info >/dev/null 2>&1; then
                sudo -n "$docker_bin" "$@"
            elif [[ -t 0 ]] && command -v sudo >/dev/null 2>&1; then
                sudo "$docker_bin" "$@"
            else
                printf '当前用户无法连接 Docker。请加入 docker 组，或在部署配置中设置 DOCKER_USE_SUDO。\n' >&2
                return 1
            fi
            ;;
        *)
            printf 'DOCKER_USE_SUDO 只能是 auto/yes/no，当前值: %s\n' "$sudo_mode" >&2
            return 2
            ;;
    esac
}
