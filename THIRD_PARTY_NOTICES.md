# Third-party notices

> 这是法律归属文件，不参与编译或运行。学习时把它理解成项目的“借书登记册”：凡是
> 直接移植或派生的第三方实现，都应在这里保留来源与许可证，不能用代码注释代替。

YJLcoder 的系统提示词与品牌为独立实现。本项目的工具工程设计参考了以下
开源项目；固定提交号用于复现本轮设计审计。

## Grok Build（xai-org/grok-build）—— 直接引入（vendor + 补丁）

- Repository: https://github.com/xai-org/grok-build
- Vendored commit: `9684fa3cdbf2995e30ea8b9b637f1db008f144fc`
- Copyright 2023-2026 SpaceXAI
- License: Apache-2.0（完整文本见 `vendor/grok-build/LICENSE`，随源码原样保留）

### 引入方式

TUI（`crates/codegen/xai-grok-pager` 及其依赖子树）整树逐字节引入于
`vendor/grok-build/`，上游文件除下述三处外不做任何修改；两处新文件为我们
所有（GPL-3.0-only，随本项目整体分发）：

1. `crates/codegen/yjl-bridge/`（新增）：实现 `acp::Agent`，把 YJLcoder 的
   Agent/会话/工具挂到该 TUI 后面。
2. 上游改动共三处（见 `patches/yjl-spawn-seam.patch`，可重放）：
   workspace members 注册桥、xai-grok-pager 依赖桥、`acp/spawn.rs` 顶部的
   接缝开关。**默认（无任何环境变量）即走 yjl-bridge，x.ai 登录层不可达**；
   仅显式设置 `YJL_NATIVE=1` 时才进入 grok 原生 MvpAgent 路径（其代码逐字
   保留，需要 x.ai 登录）。

### 许可证说明

Apache-2.0 代码并入 GPL-3.0-only 项目：按 Apache-2.0 第 4 条保留上游
版权与许可声明（本文件 + vendor 内原样 LICENSE/NOTICE），按第 6 条以
GPL-3.0-only 分发组合作品；上游文件本身未改动，仍为 Apache-2.0。
组合二进制（`vendor/grok-build/target/release/xai-grok-pager`，含桥）
按 GPL-3.0-only 提供源码。

## Claude Code Best（仅接口与交互设计参考）

- Repository: https://github.com/claude-code-best/claude-code
- Referenced commit: `3bb6b5746238c418138eb96d57765d79012edd96`
- Referenced area: `AskUserQuestionTool` 的公开输入/输出形态与交互流程；
  `FileReadTool`、`readFileInRange`、`addLineNumbers` 的范围/上限/输出语义；
  `FileReadTool/UI` 与 REPL 启动页的终端交互布局
- YJLcoder 使用独立 Rust 实现；未复制该项目的系统提示词或 TypeScript/React 源码。

## Exa MCP Server

- Repository: https://github.com/exa-labs/exa-mcp-server
- Referenced commit: `e64c11f2d3b4400ffbda8ccdd9658a450cc9d270`
- Copyright (c) 2025 Exa Labs
- License: MIT

## Pi Agent Harness

- Repository: https://github.com/earendil-works/pi
- Referenced commit: `9d2ec7ffabe927bfad2214c1cee25b6632a78dcf`
- Copyright (c) 2025 Mario Zechner
- License: MIT

## MIT License text

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
