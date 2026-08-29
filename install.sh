#!/usr/bin/env bash
# YujialeCode 一键安装（macOS / Linux）
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/ksk2kk/YujialeCode/main/install.sh | bash
set -euo pipefail

REPO_URL="https://github.com/ksk2kk/YujialeCode.git"
REPO_NAME="YujialeCode"
PROTOC_VERSION="25.3"

info()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m警告:\033[0m %s\n' "$*"; }
die()   { printf '\033[1;31m错误:\033[0m %s\n' "$*" >&2; exit 1; }

# ── 1) 定位仓库：就地优先，否则克隆/更新 ~/YujialeCode ─────────────────
if [ -f "./Cargo.toml" ] && [ -d "./vendor/grok-build" ] && [ -f "./src/main.rs" ]; then
    REPO_DIR="$(cd . && pwd)"
    info "在当前目录安装：$REPO_DIR"
else
    REPO_DIR="$HOME/$REPO_NAME"
    if [ -d "$REPO_DIR/.git" ]; then
        info "更新已有仓库：$REPO_DIR"
        git -C "$REPO_DIR" pull --ff-only || warn "git pull 失败，继续使用现有代码"
    else
        info "克隆仓库到：$REPO_DIR"
        command -v git >/dev/null 2>&1 \
            || die "缺少 git。请先安装：https://git-scm.com/downloads（mac 可 brew install git；debian/ubuntu 可 sudo apt install git）"
        git clone --depth 1 "$REPO_URL" "$REPO_DIR"
    fi
fi
cd "$REPO_DIR"

# ── 2) Rust 工具链 ──────────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
    info "未检测到 Rust，自动安装 stable 工具链（rustup）…"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || die "Rust 安装失败；请手动执行 https://rustup.rs 后重试"
info "Rust：$(cargo --version)"

# ── 3) protoc（Grok UI 构建需要）────────────────────────────────────────
if ! command -v protoc >/dev/null 2>&1; then
    OS="$(uname -s)"; ARCH="$(uname -m)"
    case "$OS:$ARCH" in
        Linux:x86_64)  PB="linux-x86_64" ;;
        Linux:aarch64|Linux:arm64) PB="linux-aarch_64" ;;
        Darwin:*)      PB="osx-universal_binary" ;;
        *) die "不支持的系统：$OS $ARCH（请手动安装 protoc https://github.com/protocolbuffers/protobuf/releases）" ;;
    esac
    mkdir -p "$HOME/.local/bin"
    info "安装 protoc $PROTOC_VERSION ($PB) 到 ~/.local/bin …"
    TMP="$(mktemp -d)"
    if curl -fsSL -o "$TMP/protoc.zip" \
        "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-${PB}.zip"; then
        if command -v unzip >/dev/null 2>&1; then
            unzip -o -q "$TMP/protoc.zip" -d "$TMP/protoc"
        elif command -v python3 >/dev/null 2>&1; then
            python3 -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$TMP/protoc.zip" "$TMP/protoc"
        else
            die "解压需要 unzip 或 python3，请先安装其一"
        fi
        install -m 755 "$TMP/protoc/bin/protoc" "$HOME/.local/bin/protoc"
        rm -rf "$TMP"
    else
        warn "protoc 下载失败（网络原因？）；若稍后 Grok UI 构建报 protoc 错误，请手动安装"
    fi
fi
export PATH="$HOME/.local/bin:$PATH"

# ── 4) 构建 yjlcoder（QQ 桥接 / 旧 TUI 用）───────────────────────────────
info "构建 yjlcoder（1/2）…"
cargo build --release

# ── 5) 构建 Grok UI（内置 YJLcoder 桥，首次约 6-15 分钟）────────────────
info "构建 Grok UI（2/2，首次编译约 6-15 分钟，请耐心等待）…"
( cd vendor/grok-build && cargo build --release -p xai-grok-pager-bin ) \
    || die "Grok UI 构建失败。常见原因：缺 protoc（第 3 步）、内存不足（可加 1GB swap）。修复后重跑本脚本即可续编。"

# ── 6) 安装 ycode 启动命令 ──────────────────────────────────────────────
mkdir -p "$HOME/.local/bin"
cat > "$HOME/.local/bin/ycode" <<LAUNCHER
#!/usr/bin/env bash
# ycode — Yujiale Code（Grok Build UI，YJLcoder agent 驱动）
set -euo pipefail
exec "$REPO_DIR/vendor/grok-build/target/release/xai-grok-pager" "\$@"
LAUNCHER
chmod +x "$HOME/.local/bin/ycode"
install -m 755 "$REPO_DIR/target/release/yjlcoder" "$HOME/.local/bin/yjlcoder" 2>/dev/null || true

case ":$PATH:" in
    *":$HOME/.local/bin:"*) PATH_OK=1 ;;
    *) PATH_OK=0 ;;
esac
if [ "$PATH_OK" = "0" ]; then
    SHELL_RC="$HOME/.bashrc"
    [ -n "${ZSH_VERSION:-}" ] && SHELL_RC="$HOME/.zshrc"
    echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$SHELL_RC"
    warn "已把 ~/.local/bin 写入 $SHELL_RC；请执行: source $SHELL_RC（或重开终端）"
fi

info "安装完成！"
printf '  启动：\033[1mycode\033[0m（Grok UI，首次进入自动弹配置向导）\n'
printf '  QQ 桥接守护：yjlcoder --qq-only\n'
printf '  旧 TUI：yjlcoder --legacy-tui\n'
printf '  目录：%s\n' "$REPO_DIR"
