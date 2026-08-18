# NapCat 运行态配置说明

本目录的 JSON 由 NapCat 读取，部分文件也会被 NapCat 自动重写。标准 JSON 不支持
注释，因此不要把 `//` 或 `_comment` 字段硬塞进运行文件；本说明作为它们的教材侧页。

## 文件生命周期

| 文件 | 用途 | 谁创建/修改 | 是否适合手写 |
|---|---|---|---|
| `napcat.json` | 通用 NapCat 行为配置 | NapCat/管理员 | 可参考，不建议运行中手改 |
| `napcat_<QQ>.json` | 指定机器人账号配置 | NapCat | 运行态生成 |
| `napcat_protocol_<QQ>.json` | 登录协议/设备相关选择 | NapCat | 运行态生成 |
| `onebot11.example.json` | OneBot v11 连接示例 | 项目维护者 | 最适合学习和复制 |
| `onebot11_<QQ>.json` | 账号实际 OneBot 配置 | NapCat/管理员 | 从示例生成后由运行环境使用 |
| `webui.json` | WebUI 监听地址和鉴权 | NapCat/管理员 | 部署时配置 |
| `passkey.json` | 登录态/密钥占位 | NapCat | 不写教材、不提交真实密钥 |

## OneBot 连接数据怎样流动

NapCat 读取 `onebot11_<QQ>.json`，作为 WebSocket 客户端连接
`ws://127.0.0.1:6701/onebot/v11/ws`。收到 QQ 消息后发出 OneBot JSON event；
`src/qq.rs` 过滤白名单并创建 Agent。Agent 回复后，qq 模块把正文包装成
`send_group_msg` 或 `send_private_msg` action，经同一条 WebSocket 发回 NapCat。

配置对象的寿命通常等于 NapCat 进程；文件修改是否热加载由 NapCat 决定，可靠做法是
修改后重启容器。账号 id、token、passkey 都属于部署数据，不应作为理解 Rust Agent
主循环的前置知识。
