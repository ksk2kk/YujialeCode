---
name: remote-job-hunter
description: 为人在中国的求职者持续搜索、去重和排序全球远程岗位，分析薪资、时区、经验要求、招聘难度、技术栈频率与学习缺口。适用于找远程工作、比较岗位、规划 Rust/C++/Go 等技术路线；不代替用户投递申请。
---

# 全球远程工作猎手

先确认或补全用户画像：技术栈与年限、英语、常驻国家/时区、可接受的雇佣形式、最低收入、夜班容忍度。优先使用 `ask_user`；缺失时使用 `references/profile.json` 的保守默认值，并在报告开头声明假设。

Skill 根目录是 `${YJLCODER_HOME:-$HOME/.yjlcoder}/skills/remote-job-hunter`。所有确定性工作必须交给其中的脚本，不要让模型手算薪资、时区、去重、频率或评分：

```sh
python3 "$SKILL_ROOT/scripts/job_hunter.py" catalog
python3 "$SKILL_ROOT/scripts/job_hunter.py" scan --profile "$SKILL_ROOT/references/profile.json" --output remote-jobs
python3 "$SKILL_ROOT/scripts/job_hunter.py" analyze --input remote-jobs/jobs.jsonl --profile "$SKILL_ROOT/references/profile.json" --output remote-jobs
```

若 shell 未设置 `SKILL_ROOT`，把上面的路径直接展开。脚本只依赖 Python 标准库。运行前用 `list_tools` 找到网页搜索工具；仅在脚本输出 `search_tasks.json` 时，用网页搜索补齐受限站点和 ATS 公司入口，再按 `references/import-schema.md` 保存为 JSONL 后执行 `import`。不得绕过登录、验证码、付费墙或网站条款。

## 必须遵守

- “远程”不等于“全球可申请”。把 Worldwide、国家限制、时区限制、工作许可分别标明；人在中国不符合的岗位不得混进“推荐”。
- 招聘页没写薪资、工时、合同期限、经验或时区时写“未公开”，不得编造。估算值必须与原始值分栏。
- 时区统一换算为北京时间，跨日写明“次日”；同时保留原时区和 UTC 偏移。
- 最终按综合推荐分排序，并分别解释技能匹配、收入、时区友好度、资格限制、新鲜度和招聘难度。招聘难度是基于要求的估计，不冒充真实申请人数。
- 每条岗位保留来源、原始申请链接、发布时间、抓取时间和字段置信度。过期、重复、纯现场和明显诈骗岗位剔除。
- 不自动投递、不上传简历、不代表用户联系招聘方，除非用户另行明确授权。

需要解释评分时读 `references/scoring.md`；需要新增或审计站点时读 `references/source-policy.md` 与 `references/sources.json`。需要修改技术词频识别时读 `references/tech-taxonomy.json`。
