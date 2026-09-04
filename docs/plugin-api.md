# YujialeCode 插件 API（`yjlcoder.plugin/v1`）

插件是**用户私有的单文件 Python 脚本或原生二进制**，放进插件目录即可被 Agent 发现和调用，无需重启、无需注册。本仓库**不内置、不分发任何插件**（`plugins/` 在 `.git/info/exclude` 中声明为本机私有、永不推送）；本文档是插件协议的完整契约，任何人（或 AI）都可以照此为自己写插件。

对照实现参考：本机 `plugins/python/` 下的用户插件（如 `eurasia_room.py`、`poc_exp.py`），以及 `plugins/bin/` 下的原生二进制插件（如 `eurasia_room_rs`，源码在 `~/.yjlcoder/plugins/rust/`）。协议的 Rust 侧实现在 `src/plugins.rs`。

## 1. 放置与发现

| 目录 | 说明 |
| --- | --- |
| `<项目根>/plugins/python/*.py` | 项目级 Python 插件，随当前工作目录生效 |
| `~/.yjlcoder/plugins/python/*.py` | 用户级 Python 插件，全局生效 |
| `<项目根>/plugins/bin/*` | 项目级原生二进制插件（Rust/Go 等） |
| `~/.yjlcoder/plugins/bin/*` | 用户级原生二进制插件 |

同名插件（manifest `name` 相同）项目级优先；同目录级别里 Python 版优先于二进制版。发现规则：

- Python 插件只认**普通 `.py` 文件**：符号链接、目录、大于 2 MiB 的文件会被拒绝并列为"可疑文件"。
- 只解析清单，**绝不执行插件代码**就能完成发现，坏插件不影响其他插件。
- 每次调用前重新扫描目录，放进去立即生效。

### 1.3 原生二进制插件（`plugins/bin`）

不需要 Python 解释器的插件形态，适合 Rust/Go 等编译型语言（更低启动开销、连接复用、无解释器依赖）：

- **可执行文件**放进 `plugins/bin/`，普通文件 + 可执行权限（unix `chmod +x`），≤ 32 MiB，不能是符号链接。
- **清单放在同名 sidecar**：`<文件名>.plugin.json`（如 `eurasia_room_rs` → `eurasia_room_rs.plugin.json`），内容是**纯 JSON** 的 manifest，字段与 §2 完全一致（二进制没有注释头，所以不吃 `# ///` 块）。
- 启动命令为 `<二进制> --yjlcoder-plugin`（无解释器、无前缀参数），协议事件、前后台生命周期、结果压缩、`--self-test` 约定与 Python 插件完全一致。
- sidecar 缺失 / JSON 无效 / 无执行权限都会在发现期报为"可疑文件"，不影响其他插件。
- 二进制同样通过 `YJLCODER_PLUGIN_DATA_DIR` 等环境变量拿数据目录（按 manifest `name` 隔离）。

## 2. Manifest（元数据块）

文件头部必须有一段 PEP 723 风格的注释块：

```python
# /// yjlcoder-plugin
# {
#   "name": "my_plugin",
#   "display_name": "我的插件",
#   "version": "1.0.0",
#   "description": "何时调用、做什么、返回什么（至少 12 个字符，写给模型看）",
#   "timeout_secs": 120,
#   "actions": [
#     {
#       "name": "do_thing",
#       "description": "做一件事，参数缺省时弹窗询问",
#       "parameters": {"type": "object", "properties": {}, "required": [], "additionalProperties": false},
#       "background": false,
#       "requires_confirmation": false
#     }
#   ]
# }
# ///
```

| 字段 | 规则 |
| --- | --- |
| `name` | 必须匹配 `[a-z][a-z0-9_]{2,47}`；这就是模型调用时的 `op` 名 |
| `display_name` / `version` | 可选，仅展示 |
| `description` | 必填且 ≥12 字符；写给模型看："何时调用、做什么、返回什么" |
| `timeout_secs` | 可选，默认 120。前台 action 钳制在 1–3600 秒；后台 action 钳制在 60 秒–30 天 |
| `actions[]` | 非空；每个 action 的 `name` 同样受正则约束且不可重复，必须有 `description`，`parameters` 必须是 JSON object |
| `background` | `true` 表示后台任务（见 §5） |
| `requires_confirmation` | `true` 表示启动前先弹用户确认卡（仅对后台 action 生效） |
| `dir_hints[]` | 可选，声明"本插件能处理哪类目录"（见 §1.1） |
| `category` | 可选，归属的系统工具分类 id（如 `"sec"`）：本插件的 actions 会动态合并进 `list_tools` 该分类页与索引行，模型把插件能力当原生工具看待（见 §1.2）。空 = 只留在 `[plugins]` |
| `category_actions` | 可选，合并进系统分类的 action 白名单；空 = 全部 actions |

### 1.1 目录感知提示（`dir_hints`）

本地模型经常在"整理某个目录"类任务里绕过插件、用 shell 手工重复造轮子。`dir_hints` 让插件声明自己的领域特征，Rust 在 `listdir` 首页结果末尾附一行可照抄的调用建议，把模型引回插件：

```json
"dir_hints": [
  {"keywords": ["cve", "poc", "exp"], "min_entries": 2, "min_ratio": 0.5,
   "label": "POC/EXP 漏洞集合",
   "suggest": "execute_command {\"op\":\"my_plugin\",\"args\":{\"action\":\"import\",\"dir\":\"{path}\"}}"}
]
```

- 命中条件：目录条目名中包含任一关键词（大小写不敏感子串）的数量 ≥ `min_entries` 且占比 ≥ `min_ratio`。
- `suggest` 里的 `{path}` 会被替换为当前列出的目录；缺省时回退为 `help` 调用示例。
- 提示只在 listdir 首页出现一次、计入输出 token 预算，Rust 侧完全插件无关（规则由各插件自带）。

### 1.2 系统分类合并（`category`）

插件默认只出现在 `[plugins]` 分类。声明 `"category": "sec"` 后，其 actions 会以 `插件名:action` 的形式合并进 `list_tools {"category":"sec"}` 的分类页，分类目录索引行也会追加 `+ 插件: 名字×N`——模型查看"网络安全"分类时直接看到完整渗透能力面，无需知道插件机制的存在。插件缺席（未安装/校验失败）时条目自动消失，Rust 侧不持有任何插件知识。

```json
"category": "sec",
"category_actions": ["fingerprint", "verify", "exploit"]
```

合并条目的展示格式（action 描述取第一句话，后台/确认语义带徽标）：

```
- poc_exp:verify — 自动 POC 验证（后台）：对授权目标先做指纹识别…；后台任务（task_wait 轮询）
  调用: execute_command {"op":"poc_exp","args":{"action":"verify"}}（详细参数: {"action":"help","for":"verify"}）
```

`category` 必须是已存在的系统分类 id（`registry.rs` 的 `CATEGORIES`），且不能是保留的 `plugins`。

模型看到的入口（`list_tools {"category":"plugins"}` 会给出提示）：

```
execute_command {"op":"<插件名>","args":{"action":"<action名>", ...参数}}
```

`action` 缺省等于 `help`。每个插件自动获得内建 action：`help`、`task_status`、`task_wait`、`task_cancel`、`task_list`，无需声明。

## 3. 调用协议（stdin → stdout）

Rust 以子进程方式启动插件：

```
python3 <插件路径> --yjlcoder-plugin
```

随即向 **stdin 写一行 JSON** 请求：

```json
{"protocol":"yjlcoder.plugin/v1","type":"invoke","request_id":"req-...","plugin":"my_plugin","action":"do_thing","args":{...},"task_id":null}
```

插件从 **stdout 每行输出一个 JSON 事件**，全部携带 `"protocol":"yjlcoder.plugin/v1"`。事件类型：

| 事件 | 用途 | 关键字段 |
| --- | --- | --- |
| `result` | **最终结果，必须发且只发一次** | `ok`(bool)、`status`("completed"/"failed"/"cancelled")、`summary`(给模型的一句话)、`data`(精简结构化结果)、`error{code,message,retryable}`(失败时) |
| `progress` | 进度（仅刷新 TUI 状态行，**不进入模型上下文**） | `message`、`phase`、`attempts`、`last_error`、任意计数器 |
| `log` | 调试日志（写日志文件，不进上下文） | `level`、`message` |
| `request_user` | 向用户弹问题卡（仅前台，见 §6） | `request_id`、`questions` |

约定：

- **stdout 只输出协议 JSON**；业务日志走 `log` 事件或 stderr（stderr 整体落日志文件）。
- 进程退出且没有 `result` 事件时，Rust 走 legacy 兜底：把 stdout 最后一行非 JSON 文本当 summary。这是兼容路径，不要依赖。
- `request_user` 的回答以一行 JSON 写回插件 stdin：`{"protocol":...,"type":"user_response","request_id":...,"outcome":{...}}`。
- 用户按 Esc 取消（前台）会收到 SIGTERM/进程组终止，插件可以捕获 KeyboardInterrupt 后发 `status:"cancelled"` 的 result 再退出。

## 4. 结果压缩（上下文预算，硬约束）

`result.data` 返回模型前会被 Rust 强制压缩：

- 任意字符串压成单行、最多 **1200 字符**；
- 数组最多 **40 项**，超出追加 `{"omitted_items":N}`；
- 嵌套最多 **8 层**；
- `trace`/`logs`/`stdout`/`stderr`/`debug` 键直接丢弃。

**设计准则：大结果落盘，返回路径。** 全量数据写到数据目录的文件里，`data` 里只放路径 + 计数 + 精简摘要；模型需要细节时用 `readline` 分页读文件。列表类结果自带 `limit/offset` 分页。

## 5. 前台 / 后台生命周期

**前台 action**（默认）：调用阻塞直到 `result`。适合秒级任务和需要弹窗的任务。

**后台 action**（`"background":true`）：Rust 立即返回给模型：

```json
{"ok":true,"status":"running","task_id":"my_plugin-173...-0001","poll_after_secs":15,"full_log":"~/.yjlcoder/plugin-logs/tasks/<id>.log"}
```

插件进程继续跑，`progress` 事件持续刷新 TUI；插件最终发出的 `result` 会被记为任务终态。模型用内建 action 轮询：

- `{"action":"task_status","task_id":"...","wait_seconds":0}` — 查看摘要（可等待状态变化，最长 120 秒）
- `{"action":"task_wait","task_id":"...","wait_seconds":30}` — 同上，默认等 30 秒
- `{"action":"task_cancel","task_id":"..."}` — 发取消信号（插件进程被进程组终止）
- `{"action":"task_list"}` — 本插件的当前任务

后台任务**不能弹窗**：`request_user` 会被 Rust 拒绝并以 `BACKGROUND_INTERACTION_NOT_ALLOWED` 失败。需要用户输入的配置请在前台 action（如 `setup`/`config`）里完成。

后台任务状态同时持久化到 `~/.yjlcoder/plugin-tasks/<task_id>.json`。

## 6. 弹窗（`request_user`，仅前台）

输出事件：

```json
{"protocol":"yjlcoder.plugin/v1","type":"request_user","request_id":"ask-...","questions":[
  {"header":"选目录","question":"要导入哪个目录的 POC/EXP？","multiSelect":false,
   "options":[{"label":"~/Downloads","description":"上次导入来源","preview":"可选"}]}
]}
```

- 最多 **4 题**；问题文本不可重复；每题自动带"Other"自由输入行。
- `options[].{label,description,preview}` 全部可选；`multiSelect`/`multi_select` 均可。
- 用户提交后，stdin 收到 `outcome`：

```json
{"outcome":"accepted","answers":{"要导入哪个目录的 POC/EXP？":["/home/me/pocs"]},
 "annotations":{"要导入哪个目录的 POC/EXP？":{"notes":"Other 输入的原文"}}}
```

`answers` 的键是**问题原文**，值是**所选标签数组**（Other 自由输入就是所输入的文本）。`{"outcome":"cancelled"}` 表示用户取消，应视为终态。

- 仅 TUI 交互模式可用；QQ 桥接等无 ask 通道的环境会直接让本次调用失败（错误信息提示改用显式参数）。

## 7. 启动前确认（`requires_confirmation`）

后台 action 声明 `"requires_confirmation":true` 后，Rust 在启动任务前先弹确认卡，内容为插件名、action 描述和参数 JSON。用户选"确认启动"才真正开始。**高危 action（利用、写入、对外发送）建议始终开启**，并把人类可读的"将要做什么/影响"写进参数（例如一个 `impact` 字符串），因为它会显示在确认卡上。

## 8. 环境变量与目录

| 变量 | 含义 |
| --- | --- |
| `YJLCODER_PLUGIN_DATA_DIR` | 插件专属数据目录（`~/.yjlcoder/plugin-data/<name>/`），配置/索引/结果都放这里 |
| `YJLCODER_PLUGIN_LOG_DIR` | 调用日志目录（`~/.yjlcoder/plugin-logs/<name>/`） |
| `YJLCODER_PLUGIN_TASK_ID` | 后台任务 id（仅后台调用时存在） |
| `YJLCODER_PLUGIN_PROTOCOL` | 固定 `yjlcoder.plugin/v1` |
| `YJLCODER_PYTHON` | 用户指定的 Python 解释器（Rust 用它启动插件） |

日志会自动脱敏 `Bearer` / `token` / `access_token` 值；敏感值也请插件自己避免直接输出。插件以当前用户权限运行，进程组独立，超时会被 SIGTERM→SIGKILL 整组清理。

## 9. `--self-test` 约定

插件应支持 `python3 <插件> --self-test`：不依赖网络和 ask 通道，用内存 fixture / 本地临时目录断言核心逻辑，末行输出 `{"ok":bool,"failures":[...]}`，失败退出码非 0。不带 `--yjlcoder-plugin` 直接运行时打印一句安装说明即可退出。

## 10. 最小可用示例

```python
#!/usr/bin/env python3
# /// yjlcoder-plugin
# {"name":"hello_probe","description":"对 URL 发起一次 GET 并返回标题，演示插件协议最小实现",
#  "actions":[{"name":"fetch_title","description":"抓取网页标题","parameters":
#   {"type":"object","properties":{"url":{"type":"string"}},"required":[]}}]}
# ///
import json, re, sys, urllib.request

def emit(t, **f):
    print(json.dumps({"protocol":"yjlcoder.plugin/v1","type":t, **f}, ensure_ascii=False), flush=True)

def main():
    req = json.loads(sys.stdin.readline())
    url = req.get("args", {}).get("url") or "https://example.com"
    try:
        html = urllib.request.urlopen(url, timeout=10).read(65536).decode("utf-8", "replace")
        title = re.search(r"<title[^>]*>(.*?)</title>", html, re.S | re.I)
        emit("result", ok=True, status="completed",
             summary=f"已获取 {url} 的标题", data={"title": title.group(1).strip()[:200] if title else ""})
    except Exception as exc:
        emit("result", ok=False, status="failed", summary=f"抓取失败: {exc}",
             error={"code":"FETCH_FAILED","message":str(exc)[:500],"retryable":True})

if "--yjlcoder-plugin" in sys.argv:
    main()
elif "--self-test" in sys.argv:
    print(json.dumps({"ok": True})); sys.exit(0)
else:
    print("This file is a YujialeCode Python plugin. Install it into plugins/python."); sys.exit(0)
```

## 11. 安全建议（写给插件作者）

- **授权范围先行**：任何会对目标发起请求的插件，都应维护一份本机 scope 配置（allowlist），范围外目标直接拒绝并告诉模型如何让用户扩权。
- **验证与利用分离**：POC 验证（无害判定）与 EXP 利用（产生效果）分成两个 action；利用永远 `requires_confirmation`，或至少要求显式参数。
- **最小上下文**：遵守 §4；把"下一步该怎么调"写进返回的 `next` 字段，模型照抄即可，无需自行推理。
- 子进程一律 `start_new_session=True`（独立进程组）+ 超时强杀，避免残留。
