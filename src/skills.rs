use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use crate::config::Config;
const SKILLS_REPO_RAW: &str = "https://raw.githubusercontent.com/anthropics/skills/main/skills";
const BUNDLED_REMOTE_JOB_HUNTER_VERSION: &str = "3";
const BUNDLED_REMOTE_JOB_HUNTER: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../skills/remote-job-hunter/SKILL.md")),
    ("references/profile.json", include_str!("../skills/remote-job-hunter/references/profile.json")),
    ("references/scoring.md", include_str!("../skills/remote-job-hunter/references/scoring.md")),
    ("references/source-policy.md", include_str!("../skills/remote-job-hunter/references/source-policy.md")),
    ("references/sources.json", include_str!("../skills/remote-job-hunter/references/sources.json")),
    ("references/ats-boards.json", include_str!("../skills/remote-job-hunter/references/ats-boards.json")),
    ("references/tech-taxonomy.json", include_str!("../skills/remote-job-hunter/references/tech-taxonomy.json")),
    ("references/import-schema.md", include_str!("../skills/remote-job-hunter/references/import-schema.md")),
    ("scripts/job_hunter.py", include_str!("../skills/remote-job-hunter/scripts/job_hunter.py")),
];
fn skills_dir(cfg: &Config) -> PathBuf {
    let d = cfg.skills_dir();
    let _ = fs::create_dir_all(&d);
    d
}
fn ensure_bundled_skills(cfg: &Config) -> Result<(), String> {
    let target = skills_dir(cfg).join("remote-job-hunter");
    let marker = target.join(".yjlcoder-bundled-version");
    if target.exists() && !marker.exists() {
        // 没有标记的同名目录属于用户，绝不覆盖。
        return Ok(());
    }
    if marker.exists()
        && fs::read_to_string(&marker).unwrap_or_default().trim()
            == BUNDLED_REMOTE_JOB_HUNTER_VERSION
        && BUNDLED_REMOTE_JOB_HUNTER
            .iter()
            .all(|(relative, _)| target.join(relative).exists())
    {
        return Ok(());
    }
    for (relative, content) in BUNDLED_REMOTE_JOB_HUNTER {
        let path = target.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建内置技能目录失败: {e}"))?;
        }
        fs::write(&path, content).map_err(|e| format!("写入内置技能 {relative} 失败: {e}"))?;
    }
    fs::write(marker, BUNDLED_REMOTE_JOB_HUNTER_VERSION)
        .map_err(|e| format!("写入内置技能版本失败: {e}"))?;
    Ok(())
}
pub fn op_list_skills(cfg: &Config) -> Result<String, String> {
    ensure_bundled_skills(cfg)?;
    let dir = skills_dir(cfg);
    let mut out = String::from("已安装技能:\n");
    let mut found = false;
    let rd = fs::read_dir(&dir).map_err(|e| format!("读取技能目录失败: {e}"))?;
    let mut items: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    items.sort();
    for p in items {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if p.is_dir() {
            found = true;
            let sk = p.join("SKILL.md");
            let desc = if sk.exists() {
                skill_frontmatter_desc(&sk)
            } else {
                String::new()                                 
            };
            out.push_str(&format!("- {name} {desc}\n"));
        } else if p.extension().map(|e| e == "md").unwrap_or(false) {
            found = true;
            out.push_str(&format!("- {}（文件）\n", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    if !found {
        out.push_str("（暂无，install_skill <name> 安装，如 install_skill pdf）\n");
    }
    Ok(out)
}
fn skill_frontmatter_desc(sk: &Path) -> String {
    let Ok(content) = fs::read_to_string(sk) else { return String::new() };                    
    let mut in_front = false;
    for line in content.lines().take(20) {
        let line = line.trim();
        if line == "---" {
            if in_front {
                break;
            }
            in_front = true;
            continue;
        }
        if let Some(desc) = line.strip_prefix("description:") {
            return desc.trim().trim_matches('"').to_string();
        }
    }
    String::new()
}
pub fn op_install_skill(args: &Value, cfg: &Config) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("缺少 name 参数")?
        .trim();
    if name.is_empty() {
        return Err("name 为空".into());
    }
    let safe_name: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if safe_name.is_empty() {
        return Err("技能名非法".into());
    }
    if safe_name == "remote-job-hunter" {
        ensure_bundled_skills(cfg)?;
        return Ok(format!(
            "内置技能 remote-job-hunter 已就绪（{}）\nrun_skill remote-job-hunter 加载使用",
            skills_dir(cfg).join("remote-job-hunter").display()
        ));
    }
    let target = skills_dir(cfg).join(&safe_name);
    if target.exists() {
        return Ok(format!("技能 {safe_name} 已安装（{target:?}）"));
    }
    if name.starts_with("http://") || name.starts_with("https://") {
        let text = fetch_url(name)?;
        let _ = fs::create_dir_all(&target);
        fs::write(target.join("SKILL.md"), text).map_err(|e| format!("写入失败: {e}"))?;
        return Ok(format!("已从 URL 安装技能 {safe_name}"));
    }
    let local = Path::new(name);
    if local.exists() {
        copy_local(local, &target)?;
        return Ok(format!("已从本地路径安装技能 {safe_name}"));
    }
    let url = format!("{SKILLS_REPO_RAW}/{safe_name}/SKILL.md");
    let text = match fetch_url(&url) {
        Ok(t) => t,
        Err(e) => {
            return Err(format!(
                "从官方技能库安装失败: {e}\n可尝试: 1) 检查技能名是否存在（如 pdf/docx/pptx）；2) 使用 URL 安装；3) 本地目录安装"
            ))
        }
    };
    fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    fs::write(target.join("SKILL.md"), text).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!(
        "已安装技能 {safe_name}（来自 anthropics/skills 官方技能库）\nrun_skill {safe_name} 加载使用"
    ))
}
fn copy_local(src: &Path, target: &Path) -> Result<(), String> {
    if src.is_dir() {
        copy_dir(src, target)?;
    } else if src.extension().map(|e| e == "md").unwrap_or(false) {
        fs::create_dir_all(target).map_err(|e| e.to_string())?;
        fs::copy(src, target.join("SKILL.md")).map_err(|e| format!("复制失败: {e}"))?;
    } else {
        return Err("本地路径需为目录或 .md 文件".into());
    }
    Ok(())
}
fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for e in fs::read_dir(src).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| format!("复制 {from:?} 失败: {e}"))?;
        }
    }
    Ok(())
}
fn fetch_url(url: &str) -> Result<String, String> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            other => other.to_string(),
        })?;
    let text = resp.into_string().map_err(|e| e.to_string())?;
    if text.len() < 50 || !text.contains('#') {
        return Err("响应不是有效的 SKILL.md".into());
    }
    Ok(text)
}
pub fn op_run_skill(args: &Value, cfg: &Config) -> Result<String, String> {
    ensure_bundled_skills(cfg)?;
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("缺少 name 参数")?;
    let p = skills_dir(cfg).join(name).join("SKILL.md");
    let content = fs::read_to_string(&p).map_err(|_| {
        format!("技能 {name} 未安装（install_skill {name} 安装，list_skills 查看）")
    })?;
    let (truncated, _) = crate::compress::truncate_middle_with_token_budget(&content, 1500);
    Ok(format!(
        "【技能 {name} 已加载】以下是技能说明，请严格按其执行:\n{truncated}"
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frontmatter_desc_parses() {
        let d = std::env::temp_dir().join(format!("yjlcoder_skill_desc_{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(
            d.join("SKILL.md"),
            "---\nname: test-skill\ndescription: 一个测试技能\n---\n# 内容\n",
        )
        .unwrap();
        let desc = skill_frontmatter_desc(&d.join("SKILL.md"));
        assert_eq!(desc, "一个测试技能");
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn install_from_local_dir() {
        let src = std::env::temp_dir().join(format!("yjlcoder_skill_src_{}", std::process::id()));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("SKILL.md"), "# 本地技能\n说明内容\n").unwrap();
        let skills_dir = std::env::temp_dir().join(format!("yjlcoder_skill_dst_{}", std::process::id()));
        let _ = fs::remove_dir_all(&skills_dir);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(skills_dir.clone());
        let r = op_install_skill(&serde_json::json!({"name": src.to_str().unwrap()}), &cfg);
        assert!(r.is_ok(), "{r:?}");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&skills_dir);
    }
    #[test]
    fn bundled_remote_job_hunter_is_installed_without_network() {
        let root = std::env::temp_dir().join(format!(
            "yjlcoder_bundled_skill_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        let list = op_list_skills(&cfg).unwrap();
        assert!(list.contains("remote-job-hunter"));
        assert!(root.join("skills/remote-job-hunter/scripts/job_hunter.py").exists());
        let loaded = op_run_skill(
            &serde_json::json!({"name": "remote-job-hunter"}),
            &cfg,
        )
        .unwrap();
        assert!(loaded.contains("全球远程工作猎手"));
        let _ = fs::remove_dir_all(&root);
    }
}
