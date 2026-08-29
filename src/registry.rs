pub struct ToolDef {
    pub name: &'static str,
    pub desc: &'static str,
    pub args: &'static str,
}
pub struct Category {
    pub id: &'static str,
    pub title: &'static str,
    pub tools: &'static [ToolDef],
}
pub const CATEGORIES: &[Category] = &[
    Category {
        id: "core",
        title: "核心操作（仅这两个原生暴露）",
        tools: &[
            ToolDef {
                name: "execute_command",
                desc: "执行 shell 命令，或以 {\"op\":\"工具名\",\"args\":{...}} 调度任意工具；文件读取一律调度 readline，禁止 cat/sed/head",
                args: "{\"cmd\":\"...\"} 或 {\"op\":\"...\",\"args\":{...}}",
            },
            ToolDef {
                name: "list_tools",
                desc: "按分类列出工具；传 category 查看该分类的参数说明",
                args: "{\"category\":\"file\"} 可选",
            },
        ],
    },
    Category {
        id: "tools",
        title: "工具注册器",
        tools: &[
            ToolDef {
                name: "make_tools",
                desc: "热注册脚本工具（sh/python3），注册即生效无需重启。\n- python3 脚本用 sys.argv[1] 读 JSON 参数\n- update_tools 覆盖，delete_tools 删除",
                args: "{\"name\":\"tool_name\",\"description\":\"何时调用、做什么、返回什么（20-500字）\",\"parameters\":{\"type\":\"object\",\"properties\":{},\"required\":[],\"additionalProperties\":false},\"language\":\"python3\",\"script\":\"#!/usr/bin/env python3\\nimport sys, json\\nprint(json.loads(sys.argv[1]))\",\"timeout_secs\":120}",
            },
        ],
    },
    Category {
        id: "file",
        title: "文件编辑",
        tools: &[
            ToolDef { name: "readline", desc: "读取文件，默认前 2000 行。\n- 长文件用 offset/limit 分页；超限明确报错，不静默截断", args: "{\"file_path\":\"/绝对路径\",\"offset\":1,\"limit\":200}" },
            ToolDef { name: "writefile", desc: "覆盖写入文件", args: "{\"path\":\"...\",\"content\":\"...\"}" },
            ToolDef { name: "editline", desc: "精确替换文件文本。\n- 自动兼容换行、尾随空格和 Unicode 标点差异", args: "{\"path\":\"...\",\"old\":\"...\",\"new\":\"...\"}" },
            ToolDef { name: "appendline", desc: "追加内容到文件末尾", args: "{\"path\":\"...\",\"content\":\"...\"}" },
            ToolDef { name: "glob", desc: "按模式匹配文件名", args: "{\"pattern\":\"**/*.rs\"}" },
            ToolDef { name: "grep", desc: "正则搜索文件内容，输出行号与上下文。\n- pattern 支持 \\| 交替；超长行自动截断不刷屏", args: "{\"pattern\":\"pricing\\|cache\",\"path\":\".\",\"glob\":\"*.rs\"}" },
            ToolDef { name: "listdir", desc: "分页列出目录；结果不折叠，按返回的 offset 继续翻页", args: "{\"path\":\".\",\"offset\":0,\"limit\":200}" },
        ],
    },
    Category {
        id: "net",
        title: "网络请求与搜索",
        tools: &[
            ToolDef { name: "web_search", desc: "聚合搜索：自动并发、去重、排序和来源质量过滤", args: "{\"query\":\"...\",\"count\":8,\"include_domains\":[],\"exclude_domains\":[]}" },
            ToolDef { name: "web_research", desc: "深度研究：自动生成互补查询，聚合排序并抓取精选正文", args: "{\"query\":\"...\",\"depth\":\"deep\",\"fetch_top\":2}" },
            ToolDef { name: "web_fetch", desc: "批量抓取网页并转纯文本；url 或 urls 均可", args: "{\"urls\":[\"https://...\"],\"max_chars\":8000}" },
            ToolDef { name: "http_get", desc: "发送 HTTP GET 并返回响应", args: "{\"url\":\"...\",\"headers\":{},\"timeout\":15}" },
            ToolDef { name: "http_headers", desc: "发送 HTTP HEAD 查看响应头", args: "{\"url\":\"...\"}" },
        ],
    },
    Category {
        id: "sec",
        title: "网络安全",
        tools: &[
            ToolDef { name: "portscan", desc: "TCP connect 端口扫描", args: "{\"host\":\"127.0.0.1\",\"ports\":\"1-1000,8080\",\"timeout_ms\":500}" },
            ToolDef { name: "dns_lookup", desc: "域名解析（A/AAAA）", args: "{\"name\":\"example.com\"}" },
        ],
    },
    Category {
        id: "skill",
        title: "技能",
        tools: &[
            ToolDef { name: "list_skills", desc: "列出已安装技能", args: "{}" },
            ToolDef { name: "install_skill", desc: "安装技能：claude-code 技能库、URL 或本地路径", args: "{\"name\":\"pdf\"}" },
            ToolDef { name: "run_skill", desc: "把技能说明加载进当前上下文", args: "{\"name\":\"pdf\"}" },
        ],
    },
    Category {
        id: "session",
        title: "会话管理",
        tools: &[
            ToolDef { name: "list_sessions", desc: "列出所有会话", args: "{}" },
            ToolDef { name: "new_session", desc: "新建并切换到新会话", args: "{\"id\":\"proj-x\"}" },
            ToolDef { name: "switch_session", desc: "切换到已有会话", args: "{\"id\":\"proj-x\"}" },
            ToolDef { name: "delete_session", desc: "删除会话", args: "{\"id\":\"old\"}" },
        ],
    },
    Category {
        id: "ctx",
        title: "上下文",
        tools: &[
            ToolDef { name: "compress", desc: "压缩当前会话上下文（把旧消息摘要化）", args: "{}" },
            ToolDef { name: "stats", desc: "查看当前上下文 token 占用", args: "{}" },
        ],
    },
    Category {
        id: "qq",
        title: "QQ 消息（桥接模式下可用）",
        tools: &[
            ToolDef { name: "qq_send", desc: "向 QQ 群/好友发送消息；处理完用户消息后用它回复", args: "{\"chat\":\"group:123456\",\"text\":\"...\"}" },
            ToolDef { name: "qq_bot", desc: "管理本机 NapCat QQ 机器人：自动启动、托管令牌并打开 WebUI 登录页。\n- 默认等同 login；action 可选 login|status|wait|start|restart|stop|webui", args: "{} 或 {\"action\":\"login|status|wait|start|restart|stop|webui\",\"wait_secs\":120}" },
            ToolDef { name: "is_admin", desc: "判断指定 QQ 号是否为管理员", args: "{\"qq\":3160168215}" },
            ToolDef { name: "add_admin", desc: "添加 QQ 管理员，重启桥接后生效", args: "{\"qq\":3160168215}" },
        ],
    },
    Category {
        id: "memory",
        title: "长期记忆（按时间自动记录）",
        tools: &[
            ToolDef { name: "memory_search", desc: "关键词搜索群记忆，返回最近匹配的摘要。\n- 需要完整内容时用 readline 读取来源文件", args: "{\"query\":\"马斯克\",\"chat\":\"group:728563593\"}" },
            ToolDef { name: "memory_write", desc: "把值得长期记住的信息写入群记忆，自动带时间戳", args: "{\"chat\":\"group:728563593\",\"content\":\"刚才搜索马斯克\"}" },
        ],
    },
    Category {
        id: "ask",
        title: "向用户提问（工具失败或意图不明时澄清）",
        tools: &[
            ToolDef { name: "ask_user_question", desc: "Ask the user one or more multiple-choice questions.\n\n- Every question automatically gets an \"Other\" choice where the user can type their own answer.\n- Put your recommended option first and append \"(Recommended)\" to its label.", args: "{\"questions\":[{\"question\":\"完整的问题?\",\"options\":[{\"label\":\"选项\",\"description\":\"选择后意味着什么\",\"preview\":\"聚焦时显示的预览(可选)\"}],\"multi_select\":false}]}" },
        ],
    },
    Category {
        id: "computer",
        title: "跨平台电脑操作（Linux Wayland / macOS / Windows）",
        tools: &[
            ToolDef {
                name: "computer_use",
                desc: "操作电脑：默认在独立桌面工作，不抢用户的鼠标、键盘或焦点。\n- isolated=Linux 独立桌面可跑任意 GUI；browser=独立 Chromium/CDP 网页操作；仅 backend=host 才控制真实桌面\n- 自动兼容常见 CUA 参数别名；actions 一次最多 200 个批量动作\n- QQ/聊天框优先 send_text + x/y，一次完成聚焦、隔离剪贴板粘贴和回车",
                args: "{\"backend\":\"isolated|browser|host\",\"action\":\"launch|observe|list_outputs|list_windows|focus_window|open_url|click|double_click|move|drag|scroll|type_text|send_text|press_key|wait|stop\",\"program\":\"firefox\",\"args\":[\"--private-window\"],\"target\":\"focused_output|output|all|region|window\",\"window_id\":5,\"x\":123,\"y\":456,\"steps\":3,\"text\":\"...\",\"submit\":true,\"keys\":\"CTRL+L\",\"url\":\"https://...\",\"actions\":[{\"type\":\"click\",\"x\":10,\"y\":20}]}",
            },
        ],
    },
    Category {
        id: "agent",
        title: "受限后台 Agent（单并发、无嵌套、本地模型错峰）",
        tools: &[
            ToolDef {
                name: "spawn_agent",
                desc: "把只读的研究/检查任务交给后台 Agent。\n- 每回合最多创建 1 个；不要拆分同一任务，也不要本回合循环轮询",
                args: "{\"task\":\"明确目标、范围、证据和期望返回内容\",\"name\":\"短名称\"}",
            },
            ToolDef {
                name: "list_agents",
                desc: "紧凑列出后台 Agent；传 id 查看状态与已有结果",
                args: "{} 或 {\"id\":\"agent-...\"}",
            },
            ToolDef {
                name: "wait_agent",
                desc: "查看后台 Agent 结果；本地模型排队时立即返回，禁止同一回合循环调用",
                args: "{\"id\":\"agent-...\",\"wait_secs\":0}",
            },
            ToolDef {
                name: "stop_agent",
                desc: "停止运行中或排队中的后台 Agent",
                args: "{\"id\":\"agent-...\"}",
            },
        ],
    },
    Category {
        id: "goal",
        title: "持续目标",
        tools: &[
            ToolDef { name: "goal", desc: "读取目标状态，或在严格完成审计后标记 complete。\n- 同一不可克服原因连续三轮才可 blocked", args: "{\"action\":\"get\"} 或 {\"action\":\"update\",\"status\":\"complete|blocked\",\"reason\":\"证据或阻塞原因\"}" },
            ToolDef { name: "fuck_master", desc: "管理主人授权的主动推进任务：触发时有管理员权限，模型空闲才经 QQ 追问。\n- action: add|list|pause|resume|now|delete", args: "{\"action\":\"add|list|pause|resume|now|delete\",\"goal\":\"推进找工作学习路线\",\"every\":\"1d\",\"chat\":\"private:123\",\"id\":\"fm-0001\"}" },
        ],
    },
];
pub fn find_category(id: &str) -> Option<&'static Category> {
    CATEGORIES.iter().find(|c| c.id == id)
}
pub fn find_tool(name: &str) -> Option<&'static ToolDef> {
    CATEGORIES.iter().flat_map(|c| c.tools.iter()).find(|t| t.name == name)
}
pub fn list_categories_text() -> String {
    let mut out = String::from("可用工具分类（调用 list_tools {\"category\":\"<id>\"} 查看详细说明）:\n");
    for c in CATEGORIES {
        let names: Vec<&str> = c.tools.iter().map(|t| t.name).collect();
        out.push_str(&format!("[{}] {}: {}\n", c.id, c.title, names.join(", ")));
    }
    out
}
pub fn list_category_text(id: &str) -> Option<String> {
    let c = find_category(id)?;
    let mut out = format!("[{}] {}:\n", c.id, c.title);
    for t in c.tools {
        out.push_str(&format!("- {} — {}\n  args: {}\n", t.name, t.desc, t.args));
    }
    Some(out)
}
pub fn native_tools_json() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "execute_command",
                "description": "Execute a shell command, or dispatch any discovered tool with {\"op\":\"<tool>\",\"args\":{...}}. File reads must dispatch readline; cat/sed/head reads are rejected in code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string", "description": "shell command to run" },
                        "op": { "type": "string", "description": "tool name to dispatch" },
                        "args": { "type": "object", "description": "arguments for the dispatched tool" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_tools",
                "description": "List available tools by category. Call with no args to see the category index, or with category to see detailed schemas.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "category": { "type": "string", "description": "category id; omit for the compact category index" }
                    }
                }
            }
        }
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_schemas_stay_collapsed() {
        // 折叠保障：模型原生只见两个调度器；任何新工具（含 ask_user_question）都走调度，
        // 不允许一次性把一堆工具 schema 塞进请求。
        let native = native_tools_json();
        let names: Vec<&str> = native
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(names, vec!["execute_command", "list_tools"]);
        let text = native.to_string();
        assert!(!text.contains("ask_user_question"));
        assert!(!text.contains("computer_use"));
        // readline 仅作为调度指引出现在描述里，不暴露为可调度工具名
        assert!(!text.contains("\"readline\""));
    }
    #[test]
    fn categories_cover_all_tools() {
        let mut names = Vec::new();
        for c in CATEGORIES {
            for t in c.tools {
                assert!(!t.name.is_empty());
                assert!(!t.desc.is_empty());
                names.push(t.name);
            }
        }
        assert!(names.contains(&"readline"));
        assert!(names.contains(&"portscan"));
        assert!(names.contains(&"install_skill"));
        assert!(names.contains(&"qq_send"));
        assert!(names.contains(&"fuck_master"));
        assert!(names.contains(&"computer_use"));
    }
    #[test]
    fn list_outputs() {
        let index = list_categories_text();
        assert!(index.contains("[file]"));
        assert!(index.contains("[sec]"));
        let file = list_category_text("file").unwrap();
        assert!(file.contains("readline"));
        assert!(list_category_text("goal").unwrap().contains("fuck_master"));
        assert!(list_category_text("computer").unwrap().contains("computer_use"));
        assert!(list_category_text("nope").is_none());
    }
    #[test]
    fn native_tools_are_exactly_two_stable_dispatchers() {
        let v = native_tools_json();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let names: Vec<&str> = arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"execute_command"));
        assert_eq!(names, vec!["execute_command", "list_tools"]);
    }
    #[test]
    fn find_tool_works() {
        assert!(find_tool("editline").is_some());
        assert!(find_tool("nonexistent").is_none());
    }
}
