# 网页搜索补充格式

把网页搜索工具返回的岗位逐行保存为 UTF-8 JSONL。每行至少包含：

```json
{"source_id":"workingnomads","source":"Working Nomads","title":"Rust Engineer","company":"Example","url":"https://example.com/job/1","description":"职位原文","location":"Remote APAC","published_at":"2026-08-24T00:00:00Z"}
```

可选字段：`employment_type`、`salary_original`、`timezone_original`、`weekly_hours`、`contract_duration`、`experience_min_years`、`expires_at`。只能复制页面明确给出的值；未知字段省略或写 `null`，不能猜。

导入命令：

```sh
python3 "$SKILL_ROOT/scripts/job_hunter.py" import \
  --input remote-jobs/jobs.jsonl \
  --import-file web-results.jsonl \
  --profile "$SKILL_ROOT/references/profile.json" \
  --output remote-jobs
```

同一岗位从多个聚合站出现时也可以全部导入，脚本会按规范化 URL 和“公司 + 标题”二次去重。
