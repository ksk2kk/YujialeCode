# YJLcoder 从零手抄教材

这份教材不是“文件清单”，而是一条能走通的学习路线。目标是：你抄完之后，能在不看
原项目的情况下画出一次消息从键盘进入、经过模型和工具、再回到屏幕的完整路径；也能
解释每个长期状态由谁拥有、为什么需要 `Arc/Mutex/Atomic/mpsc`。

> 最重要的建议：**第一行从 `Cargo.toml` 开始，第一份 Rust 源码从 `src/lib.rs`
> 开始；不要从 3000 行的 `tui.rs` 开始。** TUI 是最终组装好的驾驶舱，不是发动机原理
> 入门图。

## 1. 先看懂整台机器

一次 TUI 消息的主路径是：

```text
键盘字节
  │
  ▼
tui::KeyParser ──► Tui::handle_key ──► Action::Submit(String)
  │                                           │
  │                                           ▼
  │                                    main::TurnQueue
  │                                           │ 创建工作线程
  │                                           ▼
  │                                      Agent::run_turn
  │                                           │
  │                         ┌─────────────────┴──────────────────┐
  │                         ▼                                    ▼
  │                   SessionStore                          Llm::stream
  │                   读取/追加历史                      HTTP / SSE / Mock
  │                                                              │
  │                                     模型要求工具 ◄────────────┘
  │                                           │
  │                                           ▼
  │                       tool_compat::normalize_call
  │                                           │
  │                                           ▼
  │                            tools::execute(ToolCtx)
  │                                           │
  │                         工具结果重新加入模型上下文
  │                                           │
  └──────────── AgentEvent ◄──── 最终正文 / 错误 / 提问 ──────────┘
```

QQ 路径只是把最左边的“键盘/TUI”换成 `qq.rs` 的 OneBot WebSocket；中间仍复用同一个
`Agent`、同一个工具层和同一种会话格式。

### 1.1 三条数据公路

项目看起来文件很多，其实长期只有三条公路：

1. **用户消息公路**：`String → TurnQueue → Agent → Msg → SessionStore`。
2. **模型事件公路**：`SSE → StreamEvent → AgentEvent → Tui`。
3. **工具提问公路**：`AskRequest → Tui → AskAnswer → Agent`。

每当你遇到一个 `Sender<T>`，先问：“这里的 `T` 在哪条公路上？发送后所有权交给谁？”

## 2. Rust 生命周期的项目内版本

不用一开始背生命周期语法。先把对象分成四种寿命：

| 寿命 | 项目例子 | 比喻 | 结束时机 |
|---|---|---|---|
| 编译后整个进程 | `SYSTEM_PROMPT`、`CATEGORIES`、颜色常量 | 墙上的永久标牌 | 进程退出 |
| 整个程序/服务 | main 的 `Config`、主 `Llm`、QQ `BridgeInner` | 店铺固定设备 | main/服务退出 |
| 一个对话回合 | `Agent`、`ToolLoopGuard`、`TurnSpawn` | 一张服务工单 | Done/Error 后线程退出 |
| 一次函数调用 | `args: &Value`、`ToolCtx<'a>`、解析临时变量 | 借来的扳手 | 函数返回 |

### 2.1 `String`、`&str`、`Arc<String>` 分别意味着什么

- `String`：我拥有这段文字。结构体销毁时文字也销毁，可以安全移动到线程/通道。
- `&str`：我只临时借看。不能活得比原字符串更久，通常避免一次复制。
- `Arc<T>`：多人共有一件东西。克隆只增加“持有人计数”，最后一个持有人离开才销毁。
- `Mutex<T>`：共享物品外面的一把钥匙。一次只能一个线程拿钥匙修改。
- `AtomicBool`：只表达极小状态的电子按钮，不用拿整把 Mutex 钥匙。

### 2.2 为什么 `ToolCtx<'a>` 有生命周期

`ToolCtx` 不拥有 Config、LLM 和 SessionStore，只把它们临时绑在一条“工具腰带”上：

```rust
pub struct ToolCtx<'a> {
    pub cfg: &'a Config,
    pub store: &'a mut SessionStore,
    pub llm: &'a Llm,
    // ...
}
```

`'a` 是腰带的归还日期：工具不能把这些引用存到一个活得更久的后台对象里。尤其
`&'a mut SessionStore` 同一时间只能存在一份，从类型层阻止两个工具并发改同一本账簿。

### 2.3 参数传递的四种动作

看到函数签名时，先逐项标注：

```rust
fn example(input: String, cfg: &Config, store: &mut SessionStore, cancel: Arc<AtomicBool>)
```

- `input: String`：移动所有权；调用者之后不能再用原变量，除非事先 clone。
- `cfg: &Config`：共享只读借用；函数不能修改，返回后借用结束。
- `store: &mut SessionStore`：独占可变借用；调用期间其他代码不能访问 store。
- `cancel: Arc<AtomicBool>`：共享所有权；函数可以把克隆送进线程，原调用者仍保留一份。

## 3. 正确的手抄顺序

建议新建一个完全独立的练习目录，例如 `YJLcoder-handwritten`。不要在正在使用的项目上
边删边抄。每一阶段抄完先解释，再对照原项目，再进入下一阶段。

### 第 0 阶段：只抄地图（约 20 分钟）

1. `Cargo.toml`
2. `src/lib.rs`

抄写时要回答：

- package name 为什么会成为 `use yjlcoder::...` 的前缀？
- dependency 的 feature 为什么显式列出？
- `pub mod` 是编译期声明还是运行期对象？

练习项目此时可以先只在 `lib.rs` 写你已经创建的模块名，尚未创建的行先注释掉。

### 第 1 阶段：配置与时间（约 2 小时）

3. `src/time.rs`
4. `src/config.rs`

先抄结构体和 `Default`，再抄 `load/save`，最后抄首次配置交互。这里集中学习：

- `derive(Serialize, Deserialize)` 如何把 Rust 结构与 JSON 对应；
- `serde(default)` 如何让老配置向后兼容；
- `Option<T>` 如何表示“没有配置”而不是伪造一个数；
- `PathBuf` 为什么适合长期拥有路径，`&Path` 为什么适合函数参数。

检查点：写一个临时 `main`，打印 `Config::default()` 序列化后的 JSON，再读回来比较字段。

### 第 2 阶段：消息与会话账簿（约 3 小时）

5. 先抄 `src/llm.rs` 中的 `Msg`、`ToolCall`、`StreamEvent`、`ChatResult`、`ChatRequest`
6. `src/session.rs`

这一阶段先不要抄完整网络客户端。你的目标是让“消息是什么、如何落盘”完全独立成立。

检查点：构造两条 `Msg`，追加到临时 SessionStore，重新 load 并导出 Markdown。

### 第 3 阶段：工具目录、纠错和结果外置（约 1 天）

7. `src/registry.rs`
8. `src/tool_compat.rs`
9. `src/tool_output.rs`

顺序不能反：先有“标准工具叫什么”，再学习怎样把弱模型的错误叫法归一化，最后处理
工具输出过大。这里的函数大多是纯函数，适合抄一个测试一个。

重点例子：模型给出 `{"command":"ls"}`，兼容层如何判断它应成为
`execute_command {"cmd":"ls"}`；为什么保留 `notes` 作为纠错收据。

### 第 4 阶段：真正的工具箱（约 2～3 天）

10. `src/tools.rs`

不要一次抄 2500 行。按文件内部顺序拆为小包：

1. `QqOut/Ask*/ToolCtx/KNOWN_OPS` 数据结构；
2. `execute → execute_normalized` 总分发；
3. `readline/listdir/glob/grep` 文件只读工具；
4. `writefile/editline/appendline` 修改工具；
5. `execute_command` 与禁止 cat 的安全边界；
6. 网络、安全、session、memory、QQ、ask_user 扩展工具。

每抄一个工具，写下三句话：参数 JSON 来自哪里、它借用了 ToolCtx 的什么能力、结果回到
哪里。比如 readline：`模型 args → tool_compat → file_read → 带行号完整一页 String →
Agent 写成 tool Msg → 下一次 LLM 请求`。

### 第 5 阶段：模型网络层（约 2 天）

11. 回到 `src/llm.rs`，抄完 `Llm/RemoteClient/CancellableHttp` 和 SSE 解析

按下面顺序理解，而不是照行号硬冲：

1. `Llm` 枚举如何让 Mock/Remote 共用接口；
2. `to_openai_msg` 如何把内部 Msg 变成请求 JSON；
3. SSE 的一行 `data:` 如何变成 Delta/Reasoning/ToolCall；
4. `CancellableHttp` 如何用 worker + channel 让阻塞网络可取消；
5. `Drop` 为什么能保证断线释放 llama.cpp slot；
6. 本地路由器发现、模型重载和循环/垃圾 token 检测。

检查点：先跑 Mock；再对本地服务只做 `/models`；最后才发真实 chat completion。

### 第 6 阶段：Agent 主循环（约 2～3 天）

12. `src/prompt.rs`
13. `src/compress.rs`
14. `src/agent.rs`

这是发动机。建议把 `run_turn` 画成状态图再抄：

```text
准备历史 → 请求模型 → 收正文/工具
   ▲                    │
   │                    ├─ 最终正文 ─► 保存 ─► Done
   │                    │
   └─ 保存工具结果 ◄─ 执行工具
                         │
                         └─ 重复/失败/超预算 ─► 强制收尾
```

`ToolLoopGuard` 的寿命必须限制在单回合：如果做成全局，上一个用户合理调用过 readline，
下一个用户再读同一文件就可能被误判为循环。

### 第 7 阶段：终端驾驶舱（约 3～4 天）

15. `src/md.rs`
16. `src/tui.rs`
17. `src/main.rs`

先 Markdown、后 TUI、最后 main。TUI 内部再按这个顺序：

1. 颜色常量和 `ChatRole/ChatLine/Key/Action`；
2. `KeyParser`，把字节流变成按键；
3. 输入框编辑和 slash command；
4. AskUserQuestion 状态与上下键选择；
5. Markdown 换行和聊天视口；
6. 吉祥物状态机；
7. 完整 draw/render。

最后抄 main，因为这时每条接线的两端你都认识。重点盯住 `TurnQueue`：工作中再次提交只是
移动进 `pending`，绝不直接启动第二个前台 Agent。

### 第 8 阶段：可选扩展（约 2～3 天）

18. `src/web.rs`
19. `src/skills.rs`
20. `src/qq.rs`

web 学习聚合去重与 HTML 清洗；skills 学习文件来源统一；QQ 学习 WebSocket、Arc/Mutex
和按 chat 隔离。QQ 最大，建议最后抄，避免一开始被协议细节拖住。

### 第 9 阶段：验证与部署（约 1 天）

21. `tests/mock_e2e.rs`
22. `examples/ws_probe.rs`
23. `deploy/*.service`
24. `deploy/*.sh`
25. `deploy/napcat/docker-compose.yml`

`tests` 是教材答案，不是最后才“补几个测试”。抄测试时逐个故意破坏实现，确认测试真的
会红，再恢复。service/compose 是运行环境接线，不属于 Agent 算法核心。

## 4. 每个源码文件负责什么

| 文件 | 一句话职责 | 主要输入 | 主要输出 | 核心寿命 |
|---|---|---|---|---|
| `lib.rs` | 导出模块 | 编译器 | crate API | 编译期 |
| `main.rs` | 启动和前台线程调度 | CLI、键盘、AgentEvent | TUI 动作、工作线程 | 整个进程 |
| `config.rs` | JSON 配置和目录 | 环境变量、config.json | Config | 启动/配置变更 |
| `time.rs` | 本地时间戳 | 系统时钟 | String | 单次调用 |
| `llm.rs` | OpenAI 兼容请求/SSE | ChatRequest | ChatResult/StreamEvent | 单次请求 + 共享客户端 |
| `session.rs` | JSONL 会话账簿 | Msg | Session/文件 | 整个 Agent |
| `prompt.rs` | 短系统提示词 | 模式选择 | 静态字符串 | 整个进程 |
| `registry.rs` | 工具说明目录 | category/name | 文本/schema | 整个进程 |
| `tool_compat.rs` | 弱模型参数纠错 | 原始 op/args | NormalizedCall | 单次工具调用 |
| `tool_output.rs` | 超长结果外置 | 工具 String | 原文或预览 | 单次调用 + 持久文件 |
| `tools.rs` | 工具真实执行 | op/args/ToolCtx | Result<String,String> | 单次工具调用 |
| `compress.rs` | 上下文压缩 | 历史 Msg | 新历史 | 单次压缩 |
| `agent.rs` | 模型—工具循环 | 用户 String | AgentEvent/最终 String | 一个回合 |
| `md.rs` | Markdown 分段换行 | &str | Vec<Seg> | 一帧渲染 |
| `tui.rs` | 终端状态与绘制 | Key/AgentEvent | Action/ANSI 帧 | 整个 TUI |
| `web.rs` | 搜索/研究/抓网页 | JSON args | 文本证据 | 单次工具调用 |
| `skills.rs` | 安装/读取技能 | 名称/URL/路径 | SKILL.md 内容 | 单次调用/持久文件 |
| `qq.rs` | OneBot 聊天桥接 | WS JSON | Agent/WS action | 服务期 + 每-chat 回合 |

## 5. 关键参数追踪

### 5.1 用户输入 `input`

1. `Tui` 从输入框 `std::mem::take` 出一个 `String`；输入框变空。
2. `TurnQueue::submit(input)` 取得所有权。
3. 空闲时变成 `TurnAdmission::Start(input)`，忙时移进 `VecDeque`。
4. `start_tui_turn` 再把 String 移进 `TurnSpawn`/线程。
5. `Agent::run_turn(&input, ...)` 在回合内借用，并复制进 user `Msg` 持久化。

“移动”不是复制文字，只是把堆内存的指针、长度、容量交给下一个变量。

### 5.2 `cancel`

1. main 创建 `Arc<AtomicBool>`。
2. clone 给 Agent、网络 worker、ask_user 等等待点。
3. Ctrl+C 用 `store(true, Ordering::SeqCst)` 按下急停。
4. 各层轮询 `load`；网络层 Drop future 关闭 TCP，工具/提问等待返回错误。
5. 新回合开始前重新置 false 或创建独立标志。

这里不传 `&mut bool`，因为 UI 和 worker 在不同线程，且都要同时持有它。

### 5.3 `on_event`

`impl FnMut(AgentEvent)` 是回调：Agent 不知道 TUI 或 QQ 如何展示，只负责发事件。
之所以是 `FnMut`，是因为回调通常要修改外部计数、pending 行或通道发送状态。借用只覆盖
`run_turn` 调用，Agent 不把回调保存为长期字段。

### 5.4 `args: &Value`

模型参数先作为 JSON Value 进入 `execute`，兼容层生成拥有所有权的新 `NormalizedCall`，
再把其中 args 借给具体工具。具体工具通过 `get/as_str/as_u64` 读取；缺失、类型错误或越界
都在工具边界返回人类可读 Err，不让 `unwrap` 把整个 Agent 打崩。

### 5.5 AskUserQuestion 的双向通道

```text
Agent 工具线程                      TUI 主线程
     │                                  │
     ├── AskRequest{id,questions} ─────►│ 隐藏普通输入框，显示选项
     │                                  │ ↑/↓/Space/Enter
     │◄── AskAnswer{id,answers} ────────┤
     │ 校验 id，转成工具结果              │ 恢复普通输入框
```

`id` 是快递单号：如果用户取消后旧回答晚到，没有单号校验就会投到下一次问题。

## 6. 局部变量命名词典

源码里会反复出现这些短名字。它们不是神秘缩写：

| 名称 | 通常含义 | 典型生命周期 |
|---|---|---|
| `cfg` | 当前配置快照 | 一次函数或一个 Agent |
| `req` | 即将发送的 request | 一次模型/提问调用 |
| `resp/result` | 已返回的完整结果 | 当前分支 |
| `ev` | 一个事件/Event JSON | 一次 match |
| `tx/rx` | channel 发送端/接收端 | 通道拥有者作用域 |
| `inner` | 多线程共享的内部状态 Arc | 服务或连接线程 |
| `out` | 正在拼装的输出 String/Vec | 当前函数 |
| `buf` | 网络/文件临时缓冲 | 当前读取循环 |
| `idx/i` | 当前索引 | 当前循环 |
| `n` | 本次读取数量/解析数值 | 当前语句块 |
| `p/path` | 已解析路径 | 当前文件操作 |
| `call` | 一次标准化工具调用 | 一次 execute |
| `msgs/messages` | 当前上下文快照 | 一次回合或请求 |
| `store` | 当前会话账簿 | Agent 或 ToolCtx 借用 |

对只活一两行、含义完全由类型和循环说明的变量，不要写“`i` 是 i”式注释；应在循环上方
解释索引为什么存在、上限是什么、越界如何处理。

## 7. 手抄时的固定动作

每抄一个函数，先在纸上写四格：

1. **谁调用我？**
2. **参数从哪里来，所有权怎样传？**
3. **我修改哪些状态/外部资源？**
4. **结果交给谁，我结束后什么还活着？**

例如 `tools::file_read(args)`：

- 调用者：`execute_normalized`；
- 参数：标准化 JSON 的只读借用；
- 副作用：只读文件，不修改 Session；
- 结果：拥有所有权的带行号字符串，交给 Agent 回灌模型；函数内文件句柄和缓冲销毁。

## 8. 编译与验证节奏

每个阶段至少做：

```text
cargo fmt --check        # 语法结构和格式
cargo check              # 类型、所有权和生命周期
cargo test               # 行为回归
cargo clippy --all-targets -- -D warnings   # 完成一大阶段后再跑
```

学习时不要一次积累 1000 行再编译。Rust 编译错误通常最接近你刚刚抄错的所有权或括号；
每 20～50 行检查一次，定位成本最低。

## 9. 最适合你先手写的第一小节

今天就按下面顺序开始：

1. 手抄 `Cargo.toml` 的 `[package]` 和 dependencies 注释；
2. 手抄 `src/lib.rs` 的模块地图，但只暂时启用 `time`；
3. 完整手抄 `src/time.rs`；
4. 自己写一个 `main` 调 `now_stamp()` 并打印；
5. 不看答案，口述 `t → tm → String` 三个变量分别在哪里创建、何时销毁；
6. 再进入 `config.rs`。

这是全项目最短的闭环：输入是系统时钟，输出是可见字符串，中间同时接触安全 Rust、
`unsafe` FFI 边界、栈变量、拥有所有权的返回值和单元测试。完成它后再上复杂模块，学习
曲线会平顺很多。

## 10. 哪些文件不应该加行内注释

- `Cargo.lock`：Cargo 自动生成，手改会被覆盖且可能破坏依赖校验。
- `deploy/napcat/config/*.json`：NapCat 会重写，JSON 标准也不支持注释；字段说明放在
  `deploy/napcat/config/README.md`。
- `qrcode*.png`：二进制运行态图片，不是源码。
- 空的 `passkey.json`：运行态占位，绝不往里面写教材文字。

“整个项目都讲清楚”不等于破坏机器生成文件；对这类文件采用相邻教材说明，既覆盖学习
内容，也保持它们能被原程序读取。
