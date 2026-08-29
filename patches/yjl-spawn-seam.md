# yjl-spawn-seam.patch

把 Grok Build TUI（xai-grok-pager）接到 YJLcoder agent 后面的接缝补丁。

这是 `vendor/grok-build` 相对上游 https://github.com/xai-org/grok-build
(commit 9684fa3cdbf2995e30ea8b9b637f1db008f144fc) 的**全部**改动，共三处：

1. 根 `Cargo.toml`：workspace members 注册 `crates/codegen/yjl-bridge`（+1 行）。
2. `crates/codegen/xai-grok-pager/Cargo.toml`：依赖 `yjl-bridge`（+2 行）。
3. `crates/codegen/xai-grok-pager/src/acp/spawn.rs`：`spawn_grok_shell` 顶部的
   环境变量开关 —— 默认（无任何环境变量）即使用 yjl-bridge 的 agent，x.ai 登录层不可达；
   仅显式设置 `YJL_NATIVE=1` 时才走 grok 自带的 MvpAgent 路径（其代码逐字保留）。

新文件 `crates/codegen/yjl-bridge/`（桥本体，GPL-3.0，见其 Cargo.toml）
同样视为我们新增的内容，不改动上游任何文件。

## 升级 vendor 时重放

```bash
# 1) 用新 commit 替换 vendor/grok-build（排除 .git）
# 2) 应用本补丁：
cd vendor/grok-build && git apply ../../patches/yjl-spawn-seam.patch
# 或手工按上面三处重放（改动极小）。
# 3) 重建：cargo build --release -p xai-grok-pager-bin
```

## 品牌替换（追加）

用户可见的 "Grok Build" 品牌文案改为 "YujialeCode"（欢迎框徽标/副标题、
`--help`、`Grok Build TUI` UI 文案，共 4 个文件；对应测试断言同步）。
详见 git 历史 commit「更换品牌名」。
