---
name: remote-job-hunter
description: 为人在中国的求职者持续搜索、去重和排序全球远程岗位，优先福利好、有人带、可培训、经验要求低的实习/校招/Junior 岗位，并生成可审计的自动申请队列、学习路线和技术缺口。
---

# 全球远程工作猎手

先确认或补全用户画像：技术栈与年限、英语、常驻国家/时区、可接受的雇佣形式、最低收入、夜班容忍度。**“开始搜索”不等于允许跳过画像确认**：只要语言年限/等级或英语表达仍是 `null/unknown`，必须先用一次 `ask_user` 让用户补充；用户选择跳过时才使用 `references/profile.json` 的保守默认值，并明确说明第 0 周会重新分级。

Skill 根目录是 `${YJLCODER_HOME:-$HOME/.yjlcoder}/skills/remote-job-hunter`。所有确定性工作必须交给其中的脚本，不要让模型手算薪资、时区、去重、频率或评分：

```sh
python3 "$SKILL_ROOT/scripts/job_hunter.py" catalog
python3 "$SKILL_ROOT/scripts/job_hunter.py" scan --profile "$SKILL_ROOT/references/profile.json" --output remote-jobs
python3 "$SKILL_ROOT/scripts/job_hunter.py" analyze --input remote-jobs/jobs.jsonl --profile "$SKILL_ROOT/references/profile.json" --output remote-jobs
python3 "$SKILL_ROOT/scripts/job_hunter.py" applications --input remote-jobs/jobs.jsonl --application-policy "$SKILL_ROOT/references/application-policy.json" --output remote-jobs
```

扫描完成后必须用 `read` 工具依次读取，不得只读词频 CSV 就自行总结：

1. `remote-jobs/quality-report.md`：先确认薪资异常、重复区间与聚合站资格降级；
2. `remote-jobs/report.md`：先看 S/A 级培养型岗位，再看其余可从中国申请和待确认岗位；
3. `remote-jobs/application-plan.md`：读取申请开关、材料缺口、资格阻塞和队列下一步；实际执行逐条读取 `application_tasks.jsonl`，每一步完成后用 `application-state` 更新状态；
4. `remote-jobs/learning-roadmap.md`：读取完整 12 周路线、作品主线、技能卡和验收标准；
5. `remote-jobs/learning_backlog.csv`：需要解释优先级时，引用“明确必需/加分/仅提及”的岗位数。

最终回答至少包含：数据质量结论、3–10 个可靠候选、为什么推荐、北京时间/经验/资格、主攻语言顺序、前四周学习任务和完整路线文件位置。没有读 `learning-roadmap.md` 就禁止声称已经给出学习路线。

若 shell 未设置 `SKILL_ROOT`，把上面的路径直接展开。脚本只依赖 Python 标准库。运行前用 `list_tools` 找到网页搜索工具；仅在脚本输出 `search_tasks.json` 时，用网页搜索补齐受限站点和 ATS 公司入口，再按 `references/import-schema.md` 保存为 JSONL 后执行 `import`。不得绕过登录、验证码、付费墙或网站条款。

## 必须遵守

- “远程”不等于“全球可申请”。把 Worldwide、国家限制、时区限制、工作许可分别标明；人在中国不符合的岗位不得混进“推荐”。
- 招聘页没写薪资、工时、合同期限、经验或时区时写“未公开”，不得编造。聚合站薪资必须标成“估算”且不参与收入加分；正文明确薪资与估算值分开。
- 时区统一换算为北京时间，跨日写明“次日”；同时保留原时区和 UTC 偏移。
- 先按 S/A/B/C 职业优先级、再按综合分排序。福利好、实习/校招/Junior、0–2 年经验、明确导师/带教/培训的岗位排最前；Senior/Lead/Staff 和高年限岗位必须降级。分别解释入门、培养、福利、门槛、技能、收入、时区、资格和新鲜度证据。
- 每条岗位保留来源、原始申请链接、发布时间、抓取时间和字段置信度。过期、重复、纯现场和明显诈骗岗位剔除。
- `Worldwide`、`Remote` 和 100k–200k 一类整段值若只来自聚合站卡片，均视为线索而不是事实。可从中国受雇和招聘方薪资必须由原招聘正文或可信结构化字段证实。
- 学习优先级只统计软件工程及 Rust/C++/Go 相关岗位，并区分“明确必需、加分项、仅提及”。销售、法务、设计岗位不能污染程序员路线。
- 自动申请使用 `references/application-policy.json`。默认 `enabled=false`、`dry_run=true`：可以生成针对性材料和队列，但不点击最终提交。只有资格已证实、姓名/邮箱/简历完整、岗位属于允许优先级、策略明确设为 `enabled=true` 且 `dry_run=false` 时，记录才能进入 `queued`。
- `application_tasks.jsonl` 是执行入口：`verify_eligibility`、`collect_profile`、`review_application`、`browser_fill_preview`、`submit_application` 和 `monitor_application` 都有明确动作。浏览器或 ATS 适配器每做完一步，必须调用 `application-state --queue ... --application-id ... --state ...`，不得只在聊天里口头说完成。
- 薪资期望、签证、搬迁、法律声明、背景调查和自愿人口统计问题一律进入人工复核；不得猜测或替用户编造。申请状态只能沿脚本定义的状态机更新，每一次尝试必须保留时间、次数和结果。
- `blocked_eligibility` 不是失败：先核验人在中国能否签约和收款；`blocked_profile` 要让用户补齐材料。不得为了提高投递数量绕过闸门。

需要解释评分时读 `references/scoring.md`；需要调整申请权限、限额或资料时读 `references/application-policy.json`；需要新增或审计站点时读 `references/source-policy.md` 与 `references/sources.json`。需要修改技术词频识别时读 `references/tech-taxonomy.json`。
