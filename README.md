# YujialeCode

专门为本地模型玩家开发的 code agent。纯 Rust 实现，极致简约的系统提示词设计，30 tokens 的速度下也能流畅使用。完美兼容 Claude 风格 skill。

极简纯 Rust 本地模型 CLI Agent：零运行时依赖、手搓 TUI、低系统提示词设计。面向 DeepSeek / Ollama / LM Studio / vLLM 等任意 OpenAI 兼容端点；`--mock` 离线演示无需任何 API key 即可跑通全流程；可选接入 QQ（OneBot v11，NapCat / Lagrange），支持群聊与私聊极速响应。

## 特性

- 短系统提示词：native 模式不复述函数 schema，只说明角色、工具入口与停止条件；text 模式只补充工具格式和一个 Read 示例。详细参数由 `list_tools` 按需返回。
- 双协议工具调用：text 模式解析 ```` ```tool {...} ```` 代码块；native 模式直接注册 `readline`、`execute_command`、`list_tools`、`ask_user` 四个核心入口，其余工具动态发现。
- 弱模型兼容路由：工具名、参数名、嵌套层级、字符串 JSON、字符串数字/布尔/数组写错时自动纠正；工具选错但参数意图明确时强制改道。
- 聚合网络研究：`web_search` 并发聚合 DuckDuckGo、Bing 和已配置的 Brave/SearXNG，自动规范 URL、跨源去重、质量排序和域名过滤；`web_research` 一次调用自动做互补查询并抓取精选正文。
- 容错编辑：`editline` 先精确匹配，再兼容 CRLF、尾随空格、智能引号、Unicode 破折号和特殊空格；只允许唯一命中。
- 强制 Read 通道：兼容 `Read/read_file/readline` 与 `file_path/offset/limit`；Read 是原生工具列表第一项，默认读取前 2000 行，每页连续完整并给出精确下一页 offset。简单 `cat FILE` 在执行前硬转 Read，复杂 cat 直接拒绝。
- 不会折叠的目录枚举：`ls/dir/list_directory/listdir` 统一进入连续分页器，默认 200 项、最多 1000 项；简单的 `ls -la` 命令也自动硬转。
- 结构化提问：`ask_user` 对齐 Claude Code 的 `question/header/options/multiSelect` 协议，支持 1-4 题、单选、多选和自动 Other。
- 前台回合硬串行：生成中继续发送消息会进入 FIFO 队列；Ctrl+C、过程错误和连续 `ask_user` 都不会提前释放回合锁。
- 控制层防空转：第三次相同调用、连续三次相同结果或连续四次失败会熔断；达到工具上限或模型损坏时执行一次无工具收尾。
- 大结果外置：普通工具的超长结果完整保存到 `~/.yjlcoder/tool-results/`。
- 上下文自动压缩：移植 openai/codex 实现（见出处声明），占用超阈值自动触发，`/compress` 手动触发。
- 多会话：每会话一个 jsonl，`/new /ls /use /rm`。
- QQ 桥接：OneBot v11 反向 WS 服务端（NapCat 反向连接）或正向客户端；allowlist + 触发词 / @ 过滤，每 chat 独立会话，防抖合并、极速响应。
- 技能安装：`/install pdf` 从 anthropics/claude-code 拉取 SKILL.md，或安装任意 URL / 本地目录。

## 构建与运行

```bash
cargo build --release

# 配置 API key 后运行
yjlcoder

# 离线演示（无需 key，TUI/工具循环/压缩全流程可跑）
yjlcoder --mock

# QQ 桥接（后台）+ TUI
yjlcoder --qq

# 仅 QQ 桥接守护（无 TUI）
yjlcoder --qq-only

# 临时切换模型
yjlcoder --model deepseek-v4-flash
```

首次运行自动生成 `~/.yjlcoder/config.json`。也可用环境变量 `YJLCODER_HOME` 覆盖配置目录。

## 配置

```json
{
  "provider": {
    "base_url": "https://api.deepseek.com",
    "api_key": "",
    "model": "deepseek-v4-flash",
    "ctx_window": 1000000,
    "native_tools": true,
    "timeout_secs": 120
  },
  "qq": {
    "ws_mode": "server",
    "ws_addr": "127.0.0.1:6701",
    "ws_path": "/onebot/v11/ws",
    "groups": [],
    "users": [],
    "triggers": ["yjlcoder"],
    "need_at": true,
    "max_tokens": 1024
  },
  "tui": {
    "compress_threshold": 0.75,
    "tool_result_max_tokens": 1000
  },
  "fuckloop": true
}
```

- `provider.native_tools: true` 用原生 function calling（DeepSeek 推荐）；`false` 走 text 协议（本地小模型推荐）。
- `provider.ctx_window` 是云端模型的上下文窗口（fallback）：DeepSeek v4 系列为 1M tokens（`deepseek-v4-flash` / `deepseek-v4-pro`），填真实窗口即可，压缩阈值按此计算，不要对云端模型设过小的值。本地 llama.cpp 服务会自动检测窗口（优先于配置）；Ollama / LM Studio 检测不到，需填模型实际窗口。
- `qq.groups` / `qq.users` 为空时默认拒绝全部；群内 `triggers` 命中或 `@` 触发回复。
- 权限分离：`qq.admins`（管理员 QQ）可让 agent 操作电脑（全部工具）；非管理员只能闲聊。
- 快捷命令：`/qqadmin <QQ>` 添加管理员、`/qqgroup <群号>` 添加群，重启 QQ 桥接后生效。
- 本地模型示例：`base_url: http://localhost:11434/v1, model: qwen2.5-coder:7b, native_tools: false`（Ollama）；LM Studio 为 `http://localhost:1234/v1`。

## QQ 桥接（OneBot v11）

### Docker 部署 NapCat（推荐，一键脚本在 `deploy/napcat/`）

```bash
cd deploy/napcat && ./start.sh
```

1. 浏览器打开 http://127.0.0.1:6099（NapCat WebUI），输入 `WEBUI_TOKEN`（见 `deploy/napcat/start.sh`），用手机 QQ 扫码登录机器人账号。
2. 容器以 `host` 网络运行，登录后 NapCat 按 `onebot11_<QQ>.json`（参照 `deploy/napcat/config/onebot11.example.json` 配置，`deploy/napcat/config/` 另有已配好的文件）以反向 WebSocket 连上宿主机 `ws://127.0.0.1:6701/onebot/v11/ws`。
3. 运行 `yjlcoder --qq`（TUI + 桥接）或 `yjlcoder --qq-only`（守护）。
4. `ACCOUNT` 已填机器人 QQ 号，登录过一次后重启自动快速登录。
5. 默认触发方式：群里发 `yjlcoder 你好`，或 `@机器人 你好`（`need_at`）。

手工接入（已有 NapCat/Lagrange）：在 NapCat 中新建反向 WebSocket 连接，地址填 `ws://127.0.0.1:6701/onebot/v11/ws`。

行为细节：

- 每个群 / 私聊一个独立会话（`qq_g<群号>` / `qq_u<QQ号>`），长会话复用。
- 同 chat 生成中收到新消息会排队，只保留最新一条（防抖合并），逐条串行处理。
- chat 模式限制 `max_tokens` 追求响应极速，agent 可通过 `qq_send` 主动向任意群/好友发消息。
- 未触发消息只记录不转发。

## 上下文压缩（移植 openai/codex，Apache-2.0）

压缩模块逐行移植自 [openai/codex](https://github.com/openai/codex)（Apache-2.0），出处逐条对应：

| 本仓库 | codex 来源 |
| --- | --- |
| `approx_token_count` / `approx_bytes_for_tokens` / 中间截断 | `codex-rs/utils/string/src/truncate.rs`（4 字节约 1 token） |
| `SUMMARIZATION_PROMPT` | `codex-rs/prompts/templates/compact/prompt.md` 原文 |
| `SUMMARY_PREFIX` | `codex-rs/prompts/templates/compact/summary_prefix.md` 原文 |
| `build_compacted_history` | `codex-rs/core/src/compact.rs:639-717` |
| 上下文超限删旧重试 | `codex-rs/core/src/compact.rs:309-324` |
| `formatted_truncate_text` | `codex-rs/utils/output-truncation/src/lib.rs` |

算法：全量历史 + 压缩提示词调模型，取最后一条 assistant 输出为摘要，摘要以 `SUMMARY_PREFIX` 前缀作为最后一条 user 消息，assistant / 工具消息丢弃。手动压缩最多保留最近 20k token 的用户消息；自动压缩按当前模型窗口动态计算预算，压到触发线的约 80%。

## 多会话与技能

- 会话文件在 `~/.yjlcoder/sessions/<id>.jsonl`，`/new [id]`、`/ls`、`/use <id>`、`/rm <id>`（不能删当前）。
- `/skills` 查看已安装；`/install <name>` 从 anthropics/claude-code 仓库安装（如 `pdf`），也支持 `/install <URL>` 或本地目录；`run_skill <name>` 将 SKILL.md 注入上下文。

## 开发

```bash
cargo build                 # 零警告
cargo test --all-targets    # 单测 + --mock 端到端（工具循环、熔断、自动压缩、TUI 帧断言）
cargo clippy                # 零警告
```

TUI 帧渲染有快照式断言（`frame_visual_elements`），改视觉必须先过它。

## 架构

```
src/
  main.rs        入口：--mock / --qq / --qq-only / --model，TUI 主循环
  config.rs      ~/.yjlcoder/config.json
  prompt.rs      低系统提示词
  tool_compat.rs 弱模型工具名/参数强制兼容路由
  llm.rs         OpenAI 兼容流式客户端（SSE）+ Mock 离线模型
  registry.rs    工具分类注册表（list_tools 渲染）
  tools.rs       全部 op 实现（shell/file/net/sec/session/ctx/qq）+ 容错编辑
  web.rs         多后端聚合搜索、深度研究、批量网页抓取
  agent.rs       主循环：解析工具块 → 执行 → 回灌 → 循环（max_iter=8）
  session.rs     多会话 jsonl 存储
  compress.rs    codex 移植的上下文压缩
  tui.rs         手搓 TUI（ANSI + termios）
  qq.rs          OneBot v11 桥接（反向 WS 服务端 / 正向客户端）
  skills.rs      技能安装 / 列表 / 注入
```

线程模型：主线程渲染循环 + stdin 输入线程 + 每轮一个 agent 工作线程（`std::sync::mpsc` 通信），QQ 桥接独立线程。阻塞 I/O + std 线程，无 tokio。依赖仅 serde / serde_json / ureq / tungstenite / libc / unicode-width。

## 设计参考

工程设计参考 [claude-code-best/claude-code](https://github.com/claude-code-best/claude-code) 的工具池稳定化、超长工具结果落盘、会话恢复、权限分层和任务状态可观测性；参考 [Exa MCP Server](https://github.com/exa-labs/exa-mcp-server) 的 search/fetch 分层、查询多样化、硬过滤、去重与来源质量策略；参考 [Pi Agent Harness](https://github.com/earendil-works/pi) 的参数预处理、执行状态机、截断防护、容错编辑和大输出落盘思路。许可证与固定参考版本见 `THIRD_PARTY_NOTICES.md`。
