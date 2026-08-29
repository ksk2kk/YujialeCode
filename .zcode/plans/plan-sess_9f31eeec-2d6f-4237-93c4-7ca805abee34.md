# 一个任务、两个阶段：先照抄 grok 重做提问/填表/工具文案，再 vendor TUI + ACP 桥接

## 阶段 1：ask_user / 填表 / 工具描述 照抄 grok（改造现有代码）

### 1. 数据结构照抄（src/tools.rs）
- `AskOption {label, description, preview}` 保持；`AskQuestion` 照 grok 改为 `{question, options, multi_select: Option<bool>}`，**删除 header**（首段自动当标题）
- `AskAnswer` 从 `BTreeMap<String,String>` 改为 grok 的 outcome 模型：`Accepted{answers: 题→标签列表(有序,多选为Vec), annotations:{preview?,notes?}}` / `Cancelled`；`ChatAboutThis`/`SkipInterview` 类型照抄留位（无 plan mode，UI 不触发）
- 通道：`AskRequest{id, questions}` → `AskAnswer{id, outcome}`

### 2. ask_user 工具照抄（tools.rs + registry.rs）
- 工具名 `ask_user_question`（`tool_compat` 保留 `ask_user` 旧别名）；registry 描述逐字用 grok 原文："Ask the user one or more multiple-choice questions.\n\n- Every question automatically gets an \"Other\" choice…\n- Put your recommended option first and append \"(Recommended)\" to its label."；参数 schema：`questions[].question/options[].{label,description,preview}/multi_select`
- 结果文案逐字照抄 format.rs：accepted = "User has answered your questions: …"；cancelled = "User declined to answer the questions. Continue with the task using your best judgment, or ask different questions."——**取消从「错误」改为「成功结果」**
- 校验照抄：重复问题文本拒绝；保留 1-4 题 / 2-6 选项合理上限

### 3. TUI 提问卡片照抄 question_view（src/tui.rs）
- 一屏一题 tab 式 `[n/m]`：h/l/←/→ 切题（不循环），每题独立 cursor/滚动/自由输入状态
- 卡片 chrome：首段加粗为标题、余段暗淡封顶 5 行；面板高 max(33%,8) 起步、封顶 80%；Ctrl-F 全屏
- 选项行 `1 (●) LABEL`（单选）/`X [✓] LABEL`（多选）前缀宽 6；快捷键 1-9 续 a-f；label 列宽=最长项封顶 60% 宽；未聚焦行描述折叠一行 `…`，聚焦行整段换行展开
- Space 切换（单选再按=取消；选择清 Other）；Enter 选择并进下一题，最后一题 Enter 提交
- Other 免费输入常驻底部：单选与选项互斥；z 或直接打字进输入态，Esc 退回
- Esc 阶梯：退出输入→清当前题选择→收起卡片；Ctrl-C/Shift-X = 提交 Cancelled
- 鼠标：点击选择/切换、滚轮滚动选项区
- 提交组装：未答题省略；Other→`["Other"]`+notes；preview 注记仅单选

### 4. 填表卡片照抄 elicitation_view（setup provider 配置表单切换过去）
- 三焦点区 Fields/Editing/Actions；字段编辑器 Text/Toggle/Choice；j/k 移动、Enter/Tab 进字段或下一项、Esc 逐级退；动作区 保存/取消 按钮

### 5. 工具描述精简（registry.rs 全部 ToolDef）
- grok 风格：一句动词开头能力句 + 行为要点（截断/超时/边界），去掉长串废话；语言保持中文与系统提示词一致，仅 ask_user_question 用 grok 英文原文（模型可见）

### 6. 折叠保障（不动现有架构）
- 模型依旧只见 `execute_command` + `list_tools` 两个 schema（text/native 两模式都是），ask_user_question 走调度器；保留现有守卫测试并新增「新工具也不直出」断言

### 7. 测试
- 改造 tools.rs 既有 ask_user 测试；新增 outcome JSON 形状测试；setup 表单流程测试；`cargo test` 全绿

## 阶段 2：vendor grok TUI + ACP 桥接（接续执行）
1. 全量 vendor 到 `vendor/grok-build/`（commit 9684fa3，排除 .git，逐字节不动），THIRD_PARTY_NOTICES.md 登记 Apache-2.0 来源与改动清单，另存 `patches/yjl-spawn-seam.patch`
2. `cargo build --release -p xai-grok-pager-bin`（后台；缺 cmake/make 先装）
3. `bridge/` crate（vendored workspace 成员，其根 Cargo.toml members +1 行）：ACP agent 端——initialize `_meta`（grokShell、modelState=我们的模型列表、availableCommands=我们的 COMMANDS、xai.api_key 认证）；`session/prompt`→线程跑 `Agent::run_turn`；AgentEvent→`session/update` 流；PermRequest→`session/request_permission`；AskRequest→`x.ai/ask_user_question`（负载与 grok 完全一致，1:1 直通）；CancelNotification→cancel；session/new/load/list→SessionStore；其余 ext 方法回 METHOD_NOT_FOUND（TUI 容忍）
4. `spawn.rs` 接缝（唯一 vendor 文件改动，~15 行）：`YJL_TUI=1` 时用 bridge agent，否则原 MvpAgent 路径逐字保留
5. `scripts/yjl-tui.sh` 启动脚本 + PTY 冒烟：流式渲染、权限弹窗、提问卡片、会话续接、cancel；并验证 yjlcoder 原 TUI 无回归

先交付阶段 1（cargo test 全绿），随后直接进入阶段 2。