# 招聘来源接入政策

`sources.json` 是站点事实表。新增来源时必须填写地区、主页、访问等级、自动化状态、薪资/时区字段能力和条款备注。

## 访问等级

1. `public_api`：官方公开 JSON API，可按其归属和限流要求自动抓取。
2. `public_feed`：官方 RSS/Atom/XML，可自动抓取并保留原始链接。
3. `api_key`：官方 API 但需要用户自己的密钥；缺少密钥时生成配置提示，不报假成功。
4. `public_ats`：招聘企业主动公开的 ATS Job Board API。只抓已配置或网页研究发现的公司 board，不猜测内部职位。
5. `search_only`：需要登录、动态页面、反爬明显或没有稳定公开接口。只生成精确检索 URL，交给网页搜索工具逐页核实。
6. `restricted`：条款明确禁止抓取或 API 仅限合作方。绝不自动访问数据接口，只给用户打开的官方搜索入口。

## 质量规则

- 尊重 robots、服务条款、API attribution、速率限制和 429 Retry-After。
- 不使用代理池、验证码识别、会话 Cookie 窃取、浏览器指纹伪装或隐藏接口逆向。
- 聚合站岗位尽量回溯到企业 ATS/官网；无法回溯时保留聚合站归属。
- 同一职位跨站出现时优先级：企业官网/ATS > 有审核的专业远程站 > 综合招聘站 > 搜索结果摘要。
- 每个连接器失败都记录 `source_errors.json`，其他来源继续运行；不得因为单站失败返回空报告。
- 站点注册表与解析器分离。站点新增或失效不应破坏评分、分析和历史缓存。

## 网页研究补齐格式

网页搜索工具发现的岗位按 JSONL 保存，每行至少包含：

```json
{"source":"linkedin","title":"Senior Rust Engineer","company":"Example","url":"https://...","location":"Worldwide","description":"...","published_at":"2026-08-24","salary_text":"$80–120/hour","timezone_text":"4h overlap with CET"}
```

不要只保存搜索摘要；必须打开原始招聘页核对远程范围、仍在招聘和申请链接。无法核对的条目标记 `verification_status: "unverified"`，推荐分封顶 55。
