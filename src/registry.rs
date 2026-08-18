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
        title: "核心操作（系统提示词内建，无需查询）",
        tools: &[
            ToolDef {
                name: "execute_command",
                desc: "执行 shell 命令；或传 {\"op\":\"<工具名>\",\"args\":{...}} 调度任意工具",
                args: "{\"cmd\":\"...\"} 或 {\"op\":\"...\",\"args\":{...}}",
            },
            ToolDef {
                name: "list_tools",
                desc: "分类列出可用工具；指定 category 查看该分类工具说明",
                args: "{\"category\":\"file\"} 可选",
            },
        ],
    },
    Category {
        id: "file",
        title: "文件编辑",
        tools: &[
            ToolDef { name: "readline", desc: "Claude Code Read 式文件读取：默认前 2000 行；长文件用 offset/limit 分页，超限明确报错而不截断", args: "{\"file_path\":\"/绝对路径\",\"offset\":1,\"limit\":200}" },
            ToolDef { name: "writefile", desc: "写入文件（覆盖）", args: "{\"path\":\"...\",\"content\":\"...\"}" },
            ToolDef { name: "editline", desc: "安全替换文件文本（自动兼容换行、尾随空格和 Unicode 标点差异）", args: "{\"path\":\"...\",\"old\":\"...\",\"new\":\"...\"}" },
            ToolDef { name: "appendline", desc: "追加内容到文件末尾", args: "{\"path\":\"...\",\"content\":\"...\"}" },
            ToolDef { name: "glob", desc: "按模式匹配文件名", args: "{\"pattern\":\"**/*.rs\"}" },
            ToolDef { name: "grep", desc: "在文件/目录中搜索文本", args: "{\"pattern\":\"...\",\"path\":\".\",\"glob\":\"*.rs\"}" },
            ToolDef { name: "listdir", desc: "连续分页列出目录；结果不会被通用输出层折叠，按 Next page 的 offset 继续", args: "{\"path\":\".\",\"offset\":0,\"limit\":200}" },
        ],
    },
    Category {
        id: "net",
        title: "网络请求与搜索",
        tools: &[
            ToolDef { name: "web_search", desc: "多后端聚合搜索：自动并发、去重、排序和来源质量过滤", args: "{\"query\":\"...\",\"count\":8,\"include_domains\":[],\"exclude_domains\":[]}" },
            ToolDef { name: "web_research", desc: "一键深度研究：自动生成互补查询、聚合排序并抓取精选正文", args: "{\"query\":\"...\",\"depth\":\"deep\",\"fetch_top\":2}" },
            ToolDef { name: "web_fetch", desc: "批量抓取网页并转纯文本；url 或 urls 均可", args: "{\"urls\":[\"https://...\"],\"max_chars\":8000}" },
            ToolDef { name: "http_get", desc: "发送 HTTP GET 请求并返回响应", args: "{\"url\":\"...\",\"headers\":{},\"timeout\":15}" },
            ToolDef { name: "http_headers", desc: "发送 HTTP HEAD 请求查看响应头", args: "{\"url\":\"...\"}" },
        ],
    },
    Category {
        id: "sec",
        title: "网络安全",
        tools: &[
            ToolDef { name: "portscan", desc: "TCP 端口扫描（connect 扫描）", args: "{\"host\":\"127.0.0.1\",\"ports\":\"1-1000,8080\",\"timeout_ms\":500}" },
            ToolDef { name: "dns_lookup", desc: "域名解析（A/AAAA）", args: "{\"name\":\"example.com\"}" },
        ],
    },
    Category {
        id: "skill",
        title: "技能",
        tools: &[
            ToolDef { name: "list_skills", desc: "列出已安装技能", args: "{}" },
            ToolDef { name: "install_skill", desc: "安装主流技能（来自 anthropics/claude-code 技能库，或 URL/本地路径）", args: "{\"name\":\"pdf\"}" },
            ToolDef { name: "run_skill", desc: "加载技能说明到当前上下文", args: "{\"name\":\"pdf\"}" },
        ],
    },
    Category {
        id: "session",
        title: "会话管理",
        tools: &[
            ToolDef { name: "list_sessions", desc: "列出所有会话", args: "{}" },
            ToolDef { name: "new_session", desc: "新建并切换到会话", args: "{\"id\":\"proj-x\"}" },
            ToolDef { name: "switch_session", desc: "切换到已有会话", args: "{\"id\":\"proj-x\"}" },
            ToolDef { name: "delete_session", desc: "删除会话", args: "{\"id\":\"old\"}" },
        ],
    },
    Category {
        id: "ctx",
        title: "上下文",
        tools: &[
            ToolDef { name: "compress", desc: "压缩当前会话上下文（摘要旧消息）", args: "{}" },
            ToolDef { name: "stats", desc: "查看当前上下文 token 占用", args: "{}" },
        ],
    },
    Category {
        id: "qq",
        title: "QQ 消息（桥接模式下可用）",
        tools: &[
            ToolDef { name: "qq_send", desc: "向 QQ 群/好友发送消息（处理完用户消息后用它发送回复）", args: "{\"chat\":\"group:123456\",\"text\":\"...\"}" },
            ToolDef { name: "is_admin", desc: "判断指定 QQ 号是否为管理员", args: "{\"qq\":3160168215}" },
            ToolDef { name: "add_admin", desc: "添加 QQ 管理员（重启桥接后生效）", args: "{\"qq\":3160168215}" },
        ],
    },
    Category {
        id: "memory",
        title: "长期记忆（群记忆文件自动记录，按时间存储）",
        tools: &[
            ToolDef { name: "memory_search", desc: "关键词搜索群记忆，返回时间最近的匹配条目（每条只返回摘要；需要完整内容时用 readline 读取来源文件）", args: "{\"query\":\"马斯克\",\"chat\":\"group:728563593\"}" },
            ToolDef { name: "memory_write", desc: "把值得长期记住的信息写入群记忆（一句话即可，自动带时间戳）", args: "{\"chat\":\"group:728563593\",\"content\":\"刚才搜索马斯克\"}" },
        ],
    },
    Category {
        id: "ask",
        title: "向用户提问（工具失败或意图不明时澄清）",
        tools: &[
            ToolDef { name: "ask_user", desc: "Claude Code 式结构化提问；1-4 题、每题 2-4 个选项，界面自动提供 Other；仅 TUI", args: "{\"questions\":[{\"question\":\"...?\",\"header\":\"短标题\",\"options\":[{\"label\":\"选项\",\"description\":\"影响\"}],\"multiSelect\":false}]}" },
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
                "name": "readline",
                "description": "Read a local text file. This is the only allowed way to read file contents; never use execute_command, cat, sed, or head. Lines are 1-based and every returned page is complete. Continue from the explicit next offset when present.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "absolute or workspace-relative file path" },
                        "offset": { "type": "integer", "minimum": 1, "description": "1-based first line; defaults to 1" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "maximum lines in this complete page; defaults to 2000" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "execute_command",
                "description": "Run, build, test, or perform shell-only work. Never read files with cat/sed/head; use readline. Can dispatch other tools with {\"op\":\"<tool>\",\"args\":{...}}.",
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
                        "category": { "type": "string", "description": "category id, e.g. file, sec, net, skill" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask 1-4 structured multiple-choice questions. The UI automatically provides Other for custom text. Put a recommended option first and suffix its label with (Recommended).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "Clear, specific question" },
                                    "header": { "type": "string", "description": "Very short label, max 12 characters" },
                                    "options": {
                                        "type": "array",
                                        "minItems": 2,
                                        "maxItems": 4,
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Concise display label, 1-5 words" },
                                                "description": { "type": "string", "description": "Meaning, impact, or trade-off" },
                                                "preview": { "type": "string", "description": "Optional text preview" }
                                            },
                                            "required": ["label", "description"]
                                        }
                                    },
                                    "multiSelect": { "type": "boolean", "description": "Allow multiple selections" }
                                },
                                "required": ["question", "header", "options", "multiSelect"]
                            },
                            "description": "Questions to ask (1-4); do not add an Other option"
                        },
                        "metadata": {
                            "type": "object",
                            "properties": {
                                "source": { "type": "string", "description": "Optional source identifier" }
                            }
                        }
                    },
                    "required": ["questions"]
                }
            }
        }
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
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
    }
    #[test]
    fn list_outputs() {
        let index = list_categories_text();
        assert!(index.contains("[file]"));
        assert!(index.contains("[sec]"));
        let file = list_category_text("file").unwrap();
        assert!(file.contains("readline"));
        assert!(list_category_text("nope").is_none());
    }
    #[test]
    fn native_tools_expose_read_as_a_core_tool() {
        let v = native_tools_json();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["function"]["name"], "readline");
        let names: Vec<&str> = arr
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"ask_user"));
        assert!(names.contains(&"execute_command"));
    }
    #[test]
    fn find_tool_works() {
        assert!(find_tool("editline").is_some());
        assert!(find_tool("nonexistent").is_none());
    }
}
