# YujialeCode

[English](README.en.md) | [中文](README.md)

专门为本地模型玩家开发的 code agent。纯 Rust 实现，极致简约的系统提示词设计，30 tokens 的速度下也能流畅使用。完美兼容 Claude 风格 skill。

> **写给第一次接触本地 Agent 的你：** 你不需要先学会复杂的提示词，也不必因为模型不够聪明而放弃。YujialeCode 不靠堆叠提示词“祈祷模型听话”，而是用纯 Rust 工具层替模型兜底：自动纠正工具调用、完整读取文件、持续推进 Goal，并支持按需热注册新工具。你只需要尽量说清楚想做什么，剩下的路由、重试和失败恢复交给它。
>
> 这是一个会持续维护的长期项目。我会认真对待每一次卡住、崩溃和“不好用”，持续修复那些令人困惑的细节，也会不断吸收优秀开源项目的经验。希望本地模型 Agent 不只是少数人的玩具，而是一件普通用户也能放心拿起来使用的工具。

## 致谢

本项目以我的朋友 [Yujiale](https://github.com/dawalishi821) 的名字命名——YujialeCode 中的 "Yujiale" 正是他的名字。祝 Yujiale 生日快乐！

极简纯 Rust 本地模型 CLI Agent：依赖少（serde / ureq / reqwest / tokio / tungstenite 等 8 个 crate）、低系统提示词设计。面向 DeepSeek / Ollama / LM Studio / vLLM 等任意 OpenAI 兼容端点；`--mock` 离线演示无需任何 API key 即可跑通全流程；可选接入 QQ（OneBot v11，NapCat / Lagrange），支持群聊与私聊极速响应。

**界面已升级为 Grok Build TUI**（`ycode` 启动）：整树引入 [xai-org/grok-build](https://github.com/xai-org/grok-build) 的终端 UI（Apache-2.0，见 `vendor/`），由本项目实现的后端桥（`yjl-bridge`）驱动——无任何 x.ai 登录，提问卡片、模型选择器、插件市场、斜杠补全全部可用；首启内置 25 家供应商 + 本地服务自动探测的配置向导。原手写 TUI 保留为 `--legacy-tui`。

## 特性

- 短系统提示词：native 模式不复述函数 schema，只说明角色、工具入口与停止条件；text 模式只补充工具格式和一个 Read 示例。详细参数由 `list_tools` 按需返回。
- 双协议工具调用：text 模式解析 ```` ```tool {...} ```` 代码块；native 模式直接注册 `readline`、`execute_command`、`list_tools`、`ask_user` 四个核心入口，其余工具动态发现。
- 不打扰用户的 Computer Use：默认不再碰真实桌面。Linux 每个会话拥有独立的 headless Sway compositor（独立 Wayland socket、seat、焦点、键盘和虚拟指针），可运行任意 GUI；网页可选独立 Chromium/CDP，成本更低。原有 Linux Wayland、macOS、Windows 宿主桌面后端仍保留，但必须明确传 `backend=host`。连续动作可一次提交，最后只送一张截图给本地视觉模型。
  - 原理、成熟方案对比和平台边界见 [Computer Use 隔离设计](docs/computer-use-isolation.md)。
- 主动推进目标：`/FuckMaster` 可创建定时跟进；Agent 忙碌时提醒进入等待队列，空闲后再通过 TUI/QQ 主动询问进度。QQ 输出在代码层统一转成无 Markdown、无 emoji 的纯文本。
- 脚本不再像“黑盒”：命令先显示，运行时原位刷新最近输出、耗时、行数和体积；Esc 或超时会清理整棵进程树，脚本也不会和输入框抢键盘。
- 弱模型兼容路由：工具名、参数名、嵌套层级、字符串 JSON、字符串数字/布尔/数组写错时自动纠正；工具选错但参数意图明确时强制改道。
- 聚合网络研究（免费三层）：`web_search` 零 key 开箱即用——并发聚合 Bing（国际结果）、DuckDuckGo（html/lite 双端点轮换）和 Wikipedia 官方 API，单引擎被反爬拦截时自动退避重试换端点，连续失败自动冷却 5 分钟，结果去广告跟踪链、跨源去重、质量排序和域名过滤；可选免费 key（Brave / Tavily）自动加入聚合池；终极免费稳定方案 `bash deploy/searxng/start.sh` 一键自托管本地 SearXNG（自动探测纳入）。`web_fetch` 正文抓取失败或遇到 JS 渲染壳时自动经 Jina Reader 免费转换（内网地址不外发）；`web_research` 一次调用自动做互补查询并抓取精选正文。
- 容错编辑：`editline` 先精确匹配，再兼容 CRLF、尾随空格、智能引号、Unicode 破折号和特殊空格；只允许唯一命中。
- 强制 Read 通道：兼容 `Read/read_file/readline` 与 `file_path/offset/limit`；Read 是原生工具列表第一项，默认读取前 2000 行，每页连续完整并给出精确下一页 offset。简单 `cat FILE` 在执行前硬转 Read，复杂 cat 直接拒绝。
- 不会折叠的目录枚举：`ls/dir/list_directory/listdir` 统一进入连续分页器，默认 200 项、最多 1000 项；简单的 `ls -la` 命令也自动硬转。
- 结构化提问：`ask_user_question` 对齐 Grok Build 协议（`question/options[{label,description,preview}]/multi_select`，一屏一题卡片、单选 `(●)` 多选 `[x]`、Other 免输入行、Esc 阶梯取消；取消是成功结果，模型收到"按最佳判断继续"）。
- Grok Build UI 深度兼容：供应商向导（25 家注册表 + 本地探测，选完自动填端点/密钥/模型）、`/model` 切换真实落盘、`/plugin` 插件市场（安装走 grok 原生编排，装完自动把技能同步给本 agent）、Skills 面板、`/skills` `/cfg show` `/server` `/apikey` `/ctx` `/budget` `/price` `/save` `/ls` `/new` `/use` `/rm` `/stats` `/goal` `/FuckMaster` `/qqadmin` 等斜杠命令全部接入。
- 前台回合硬串行：生成中继续发送消息会进入 FIFO 队列；Ctrl+C、过程错误和连续 `ask_user` 都不会提前释放回合锁。
- 控制层防空转：第三次相同调用、连续三次相同结果或连续四次失败会熔断；达到工具上限或模型损坏时执行一次无工具收尾。
- 大结果外置：普通工具的超长结果完整保存到 `~/.yjlcoder/tool-results/`。
- 上下文自动压缩：移植 openai/codex 实现（见出处声明），占用超阈值自动触发，`/compress` 手动触发。
- 多会话：每会话一个 jsonl，`/new /ls /use /rm`。
- QQ 桥接：OneBot v11 反向 WS 服务端（NapCat 反向连接）或正向客户端；allowlist + 触发词 / @ 过滤，每 chat 独立会话，防抖合并、极速响应。
- `qq_bot` 系统工具：模型空参数调用即可启动/接管本机 NapCat、生成或导入 WebUI 令牌、打开原生 WebUI 扫码页并判断登录状态；不会复制或刷新二维码，令牌只保存在本机权限为 0600 的文件中。
- 技能安装：`/install pdf` 从 anthropics/claude-code 拉取 SKILL.md，或安装任意 URL / 本地目录。

## 构建与运行

**一键安装**（自动处理 Rust / protoc / 克隆 / 双构建 / 安装 `ycode` 命令）：

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/ksk2kk/YujialeCode/main/install.sh | bash
```

```powershell
# Windows（PowerShell）
irm https://raw.githubusercontent.com/ksk2kk/YujialeCode/main/install.ps1 | iex
```

**手动构建：**

```bash
# 1) 构建主程序
cargo build --release

# 2) 构建 Grok UI（需要 protoc；一次即可，~6 分钟）
cd vendor/grok-build && cargo build --release -p xai-grok-pager-bin && cd ..

# 启动（推荐装个短命令：install -m 755 scripts/ycode ~/.local/bin/ycode）
ycode          # 或 yjlcoder / 直接跑二进制，默认进入 Grok Build UI

# 首次启动自动弹配置向导：本地服务探测 + 25 家供应商（DeepSeek/OpenAI/
# Anthropic/Gemini/Grok/Kimi/Qwen/GLM/OpenRouter/Groq…），选完自动配置

yjlcoder --legacy-tui     # 旧版手写 TUI（弃用，保留备用）
yjlcoder --mock           # 离线演示（无需 key，旧 TUI 全流程可跑）
yjlcoder --qq             # Grok UI + 后台 QQ 桥接
yjlcoder --qq-only        # 仅 QQ 桥接守护（无 TUI）
yjlcoder --model <name>   # 临时切换模型
```

首次运行自动生成 `~/.yjlcoder/config.json`；也可用环境变量 `YJLCODER_HOME` 覆盖配置目录。Grok UI 想看原生 x.ai 模式（需要登录）：`YJL_NATIVE=1 ycode`。

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
- `search` 网络搜索全部免费、默认零 key：`web_search` 的 auto 池 = Bing + DuckDuckGo（`ddg_endpoints` 可自定义镜像，默认 html/lite 双端点）+ Wikipedia，自动重试换端点、连续失败冷却 5 分钟、过滤广告链接；可选 `brave_key` / `tavily_key`（各家免费额度以官网为准）自动加入聚合池；`searxng_url` 或本地自托管（`bash deploy/searxng/start.sh` 一键启动，自动探测 127.0.0.1:8888）；`bing_ensearch: false` 关闭 Bing 国际结果；`timeout_secs` 免费引擎请求超时（默认 15）。
- `provider.ctx_window` 是云端模型的上下文窗口（fallback）：DeepSeek v4 系列为 1M tokens（`deepseek-v4-flash` / `deepseek-v4-pro`），填真实窗口即可，压缩阈值按此计算，不要对云端模型设过小的值。本地 llama.cpp 服务会自动检测窗口（优先于配置）；Ollama / LM Studio 检测不到，需填模型实际窗口。
- `qq.groups` / `qq.users` 为空时默认拒绝全部；群内 `triggers` 命中或 `@` 触发回复。
- 权限分离：`qq.admins`（管理员 QQ）可让 agent 操作电脑（全部工具）；非管理员只能闲聊。
- 快捷命令：`/qqadmin <QQ>` 添加管理员、`/qqgroup <群号>` 添加群，重启 QQ 桥接后生效。
- 本地模型示例：`base_url: http://localhost:11434/v1, model: qwen2.5-coder:7b, native_tools: false`（Ollama）；LM Studio 为 `http://localhost:1234/v1`。

## QQ 桥接（OneBot v11）

### Docker 部署 NapCat（推荐，一键脚本在 `deploy/napcat/`）

```bash
mkdir -p ~/.config/yjlcoder
install -m 600 deploy/deploy.env.example ~/.config/yjlcoder/deploy.env
# 编辑 deploy.env，填写自己的 token；QQ 号可以留空后扫码
deploy/napcat/start.sh --no-follow
```

1. `deploy.env` 是唯一的机器私有配置：二进制路径、QQ 号、WebUI token、容器名、数据目录、端口和 OneBot 地址都可修改，也可给脚本传 `--config FILE`。
2. 浏览器打开配置中的 NapCat WebUI 地址，用手机 QQ 扫码登录；真实 token 不进入仓库和脚本。
3. 登录后 NapCat 按 `YJLCODER_ONEBOT_WS_URL` 反向连接 YJLcoder；手工配置可参考 `deploy/napcat/config/onebot11.example.json`。
4. 运行 `deploy/install-user-services.sh --enable` 可安装无用户名、无仓库路径依赖的 systemd 用户服务。
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

# Grok UI 桥接端到端（PTY + mock LLM；需先构建 vendor TUI）
python3 scripts/e2e_yjl_tui.py          # 基础对话
python3 scripts/e2e_ask_yjl_tui.py      # ask_user 卡片往返
python3 scripts/e2e_onboard_yjl_tui.py  # 首启配置向导全流程
```

TUI 帧渲染有快照式断言（`frame_visual_elements`），改视觉必须先过它。

## 架构

```
src/
  main.rs        入口：默认 exec Grok UI；--legacy-tui/--mock 旧 TUI；--qq/--qq-only QQ 桥接
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

线程模型：主线程渲染循环 + stdin 输入线程 + 每轮一个 agent 工作线程（`std::sync::mpsc` 通信），QQ 桥接独立线程。其余阻塞 I/O 用 std 线程；仅 llm.rs 用单线程 tokio runtime 跑可取消的 reqwest 流式 SSE 请求。直接依赖 8 个：serde / serde_json / ureq / reqwest / tokio / tungstenite / libc / unicode-width。

### Grok UI 桥接（vendor/grok-build + yjl-bridge）

```
vendor/grok-build/                     # xai-org/grok-build 整树引入（除三处小接缝外逐字节不变）
  crates/codegen/yjl-bridge/           # 本项目新增：实现 acp::Agent，把 yjlcoder 后端挂到 TUI 后面
  crates/codegen/xai-grok-pager/       # TUI 本体；acp/spawn.rs 顶部接缝默认走 yjl-bridge（无登录层）
patches/yjl-spawn-seam.patch           # 相对上游的全部改动（可重放，升级 vendor 时用）
```

桥通过进程内 ACP 通道与 TUI 通信：`session/prompt` → `Agent::run_turn`（流式/工具/审批/提问全映射），`x.ai/ask_user_question` / `session/request_permission` 反向弹原生卡片，`x.ai/plugins/*`、`x.ai/skills/list`、`session/set_model` 等扩展方法支撑市场面板与模型切换。上游改动共三处（workspace members +1 行、pager 依赖 +2 行、spawn.rs 接缝 15 行），完整对照见 `THIRD_PARTY_NOTICES.md`。

## 设计参考

工程设计参考 [claude-code-best/claude-code](https://github.com/claude-code-best/claude-code) 的工具池稳定化、超长工具结果落盘、会话恢复、权限分层和任务状态可观测性；参考 [Exa MCP Server](https://github.com/exa-labs/exa-mcp-server) 的 search/fetch 分层、查询多样化、硬过滤、去重与来源质量策略；参考 [Pi Agent Harness](https://github.com/earendil-works/pi) 的参数预处理、执行状态机、截断防护、容错编辑和大输出落盘思路。固定参考版本见 `THIRD_PARTY_NOTICES.md`。

## 许可证

本项目采用 [GPL-3.0-only](LICENSE) 协议。

注意：
- `vendor/grok-build/` 来自 [xai-org/grok-build](https://github.com/xai-org/grok-build)（Apache-2.0，commit `9684fa3`），按第 4 条保留其版权与许可声明；Apache-2.0 与 GPLv3 兼容，组合二进制随本项目以 GPL-3.0-only 提供源码。
- 上下文压缩模块移植自 [openai/codex](https://github.com/openai/codex)（Apache-2.0），出处声明见上文表格。
