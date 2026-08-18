use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::Duration;
use crate::config::Config;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const DDG_BASE: &str = "https://html.duckduckgo.com";
const BING_BASE: &str = "https://www.bing.com/search";
const BRAVE_BASE: &str = "https://api.search.brave.com/res/v1/web/search";
const MAX_COUNT: usize = 25;
const MAX_QUERIES: usize = 4;
const MAX_FETCH_URLS: usize = 10;
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
#[derive(Debug, Clone)]
struct RankedResult {
    title: String,
    url: String,
    snippet: String,
    sources: Vec<String>,
    query_hits: HashSet<usize>,
    best_rank: usize,
    score: i64,
}
pub fn web_search(args: &Value, cfg: &Config) -> Result<String, String> {
    let queries = queries_from_args(args, false)?;
    let count = args.get("count").and_then(|c| c.as_u64()).unwrap_or(8) as usize;
    let count = count.clamp(1, MAX_COUNT);
    let backend = args.get("backend").and_then(|b| b.as_str()).unwrap_or("auto");
    let include_domains = string_list(args, "include_domains");
    let exclude_domains = string_list(args, "exclude_domains");
    let must_include = string_list(args, "must_include");
    let ranked = aggregate_search(
        &queries,
        count,
        backend,
        cfg,
        &include_domains,
        &exclude_domains,
        &must_include,
    )?;
    Ok(render_ranked(&queries, backend, &ranked[..ranked.len().min(count)]))
}
pub fn web_research(args: &Value, cfg: &Config) -> Result<String, String> {
    let queries = queries_from_args(args, true)?;
    let count = args.get("count").and_then(|c| c.as_u64()).unwrap_or(12) as usize;
    let count = count.clamp(3, MAX_COUNT);
    let backend = args.get("backend").and_then(|b| b.as_str()).unwrap_or("auto");
    let include_domains = string_list(args, "include_domains");
    let exclude_domains = string_list(args, "exclude_domains");
    let must_include = string_list(args, "must_include");
    let ranked = aggregate_search(
        &queries,
        count,
        backend,
        cfg,
        &include_domains,
        &exclude_domains,
        &must_include,
    )?;
    let visible = &ranked[..ranked.len().min(count)];
    let mut out = String::from("深度研究（自动多角度检索、去重和质量排序）\n");
    out.push_str(&render_ranked(&queries, backend, visible));
    let fetch_top = args.get("fetch_top").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
    let fetch_top = fetch_top.min(5).min(visible.len());
    if fetch_top > 0 {
        let max_chars = args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(4000) as usize;
        out.push_str("\n\n===== 精选来源正文 =====\n");
        let mut fetched = Vec::new();
        std::thread::scope(|scope| {
            let handles: Vec<_> = visible
                .iter()
                .take(fetch_top)
                .cloned()
                .map(|result| scope.spawn(move || {
                    let body = web_fetch_one(&result.url, max_chars);
                    (result, body)                       
                }))
                .collect();
            for handle in handles {
                if let Ok(result) = handle.join() {
                    fetched.push(result);
                }
            }
        });                                      
        for (index, (result, body)) in fetched.into_iter().enumerate() {
            out.push_str(&format!("\n--- 来源 {}: {} ---\nURL: {}\n", index + 1, result.title, result.url));
            match body {
                Ok(text) => out.push_str(&text),
                Err(error) => out.push_str(&format!("抓取失败: {error}")),
            }
            out.push('\n');
        }
    }
    Ok(out)
}
fn queries_from_args(args: &Value, research: bool) -> Result<Vec<String>, String> {
    let mut queries = string_list(args, "queries");
    if queries.is_empty() {
        if let Some(query) = args.get("query").and_then(Value::as_str).map(str::trim).filter(|q| !q.is_empty()) {
            queries.push(query.to_string());
        }
    }
    queries.retain(|q| !q.trim().is_empty());
    queries.truncate(MAX_QUERIES);
    if queries.is_empty() {
        return Err("缺少参数: query（也可传 queries 数组）".into());
    }
    let depth = args.get("depth").and_then(Value::as_str).unwrap_or(if research { "deep" } else { "quick" });
    let deep = research && !matches!(depth, "quick" | "shallow");
    if deep && queries.len() == 1 {
        let base = queries[0].clone();
        let has_cjk = base.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        if has_cjk {
            queries.push(format!("{base} 官方 原始资料"));
            queries.push(format!("{base} 实践 评测 局限 争议"));
        } else {
            queries.push(format!("{base} official primary source documentation"));
            queries.push(format!("{base} independent review limitations evidence"));
        }
    }
    let must = string_list(args, "must_include");
    if !must.is_empty() {
        for query in &mut queries {
            for term in &must {
                if !query.contains(term) {
                    query.push_str(&format!(" \"{term}\""));
                }
            }
        }
    }
    Ok(queries)
}
fn string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Some(Value::String(value)) => value
            .split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}
fn aggregate_search(
    queries: &[String],
    count: usize,
    backend: &str,
    cfg: &Config,
    include_domains: &[String],
    exclude_domains: &[String],
    must_include: &[String],
) -> Result<Vec<RankedResult>, String> {
    let backends = resolve_backends(backend, cfg);
    let per_job = count.clamp(5, MAX_COUNT);
    let mut jobs = Vec::new();
    for (query_index, query) in queries.iter().enumerate() {
        let filtered_query = query_with_domain_filters(query, include_domains, exclude_domains);
        for backend in &backends {
            jobs.push((query_index, filtered_query.clone(), backend.clone()));
        }
    }
    let mut completed = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(query_index, query, backend)| {
                scope.spawn(move || {
                    let result = fetch_backend(&backend, &query, per_job, cfg);
                    (query_index, backend, result)
                })
            })
            .collect();
        for handle in handles {
            if let Ok(result) = handle.join() {
                completed.push(result);
            }
        }
    });
    let mut successes = 0usize;                       
    let mut errors = Vec::new();                     
    let mut merged: HashMap<String, RankedResult> = HashMap::new();
    for (query_index, source, result) in completed {
        let results = match result {
            Ok(results) => {
                successes += 1;
                results
            }
            Err(error) => {
                errors.push(format!("{source}: {error}"));
                continue;
            }
        };
        for (rank, result) in results.into_iter().enumerate() {
            let canonical = canonical_url(&result.url);
            if canonical.is_empty()
                || !domain_allowed(&canonical, include_domains, exclude_domains)
                || !terms_allowed(&result, must_include)
            {
                continue;
            }
            let entry = merged.entry(canonical.clone()).or_insert_with(|| RankedResult {
                title: result.title.clone(),
                url: canonical,
                snippet: result.snippet.clone(),
                sources: Vec::new(),
                query_hits: HashSet::new(),
                best_rank: rank,
                score: 0,
            });
            if !entry.sources.contains(&source) {
                entry.sources.push(source.clone());
            }
            entry.query_hits.insert(query_index);
            entry.best_rank = entry.best_rank.min(rank);
            if result.title.len() > entry.title.len() {
                entry.title = result.title;
            }
            if result.snippet.len() > entry.snippet.len() {
                entry.snippet = result.snippet;
            }
        }
    }
    if successes == 0 {
        return Err(format!("所有搜索后端均失败: {}", errors.join(" | ")));
    }
    let query_text = queries.join(" ").to_lowercase();
    let mut ranked: Vec<RankedResult> = merged.into_values().collect();
    for result in &mut ranked {
        result.score = (result.sources.len() as i64 * 180)                                           
            + (result.query_hits.len() as i64 * 240)                                             
            + (80_i64.saturating_sub(result.best_rank as i64 * 6))                                                
            + domain_quality(&result.url)                                                            
            + query_coverage(&query_text, result);                                          
    }
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.best_rank.cmp(&b.best_rank)).then_with(|| a.url.cmp(&b.url)));
    Ok(ranked)
}
fn render_ranked(queries: &[String], backend: &str, results: &[RankedResult]) -> String {
    if results.is_empty() {
        return format!("搜索 \"{}\"（{backend}）: 未找到符合过滤条件的结果", queries[0]);
    }
    let mut out = format!(
        "搜索 \"{}\"（{} 个查询，聚合 {backend}，{} 条）:\n",
        queries[0],
        queries.len(),
        results.len()
    );
    for (index, result) in results.iter().enumerate() {
        let title = if result.title.trim().is_empty() { "（无标题）" } else { &result.title };
        out.push_str(&format!(
            "{}. {}\n   {}\n   {}\n   [来源: {}；命中查询: {}]\n",
            index + 1,                              
            title,
            result.url,                              
            result.snippet,
            result.sources.join("+"),                        
            result.query_hits.len()                          
        ));
    }
    out.push_str("提示: 要一键交叉检索并读取正文，调用 web_research；指定页面可用 web_fetch 批量抓取");
    out
}
fn query_with_domain_filters(query: &str, include: &[String], exclude: &[String]) -> String {
    let mut out = query.to_string();
    if let Some(domain) = include.first() {
        out.push_str(&format!(" site:{}", clean_domain(domain)));
    }
    for domain in exclude.iter().take(4) {
        out.push_str(&format!(" -site:{}", clean_domain(domain)));
    }
    out
}
fn clean_domain(domain: &str) -> String {
    domain
        .trim()                                         
        .trim_start_matches("https://")                      
        .trim_start_matches("http://")                                     
        .trim_start_matches("www.")                      
        .trim_end_matches('/')                          
        .to_lowercase()                                              
}
fn host_of(url: &str) -> &str {
    let tail = url.split_once("://").map(|(_, tail)| tail).unwrap_or(url);
    tail.split(['/', '?', '#']).next().unwrap_or("").split('@').next_back().unwrap_or("").split(':').next().unwrap_or("")
}
fn domain_matches(host: &str, domain: &str) -> bool {
    let domain = clean_domain(domain);
    host == domain || host.ends_with(&format!(".{domain}"))
}
fn domain_allowed(url: &str, include: &[String], exclude: &[String]) -> bool {
    let host = host_of(url).trim_start_matches("www.").to_lowercase();
    (include.is_empty() || include.iter().any(|domain| domain_matches(&host, domain)))
        && !exclude.iter().any(|domain| domain_matches(&host, domain))
}
fn terms_allowed(result: &SearchResult, must_include: &[String]) -> bool {
    if must_include.is_empty() {
        return true;                      
    }
    let haystack = format!("{} {} {}", result.title, result.url, result.snippet).to_lowercase();
    must_include.iter().all(|term| haystack.contains(&term.to_lowercase()))
}
fn canonical_url(url: &str) -> String {
    let url = unescape_entities(url.trim());
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return String::new();
    }
    let without_fragment = url.split('#').next().unwrap_or(&url);
    let Some((base, query)) = without_fragment.split_once('?') else {
        return without_fragment.trim_end_matches('/').to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|part| {
            let key = part.split('=').next().unwrap_or("").to_ascii_lowercase();
            !key.starts_with("utm_") && !matches!(key.as_str(), "fbclid" | "gclid" | "ref" | "ref_src" | "source")
        })
        .collect();
    let base = base.trim_end_matches('/');
    if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) }
}
fn domain_quality(url: &str) -> i64 {
    let host = host_of(url).to_lowercase();
    if host.ends_with(".gov") || host.contains(".gov.") || host.ends_with(".edu") || host.contains(".edu.") {
        100
    } else if host == "github.com" || host.ends_with(".github.com") || host == "docs.rs" || host.starts_with("docs.") || host.starts_with("developer.") {
        70
    } else if ["pinterest.", "quora.", "zhidao.", "csdn.net"].iter().any(|bad| host.contains(bad)) {
        -80
    } else {
        0
    }
}
fn query_coverage(query: &str, result: &RankedResult) -> i64 {
    let haystack = format!("{} {}", result.title, result.snippet).to_lowercase();
    let terms: HashSet<&str> = query
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|term| term.chars().count() >= 2)
        .collect();
    terms.iter().filter(|term| haystack.contains(**term)).count() as i64 * 8
}
fn resolve_backends(backend: &str, cfg: &Config) -> Vec<String> {
    match backend {
        "ddg" => vec!["ddg".into()],
        "bing" => vec!["bing".into()],
        "brave" => vec!["brave".into()],
        "searxng" => vec!["searxng".into()],
        _ => {
            let mut v = vec!["ddg".into(), "bing".into()];
            if !cfg.search.brave_key.is_empty() {
                v.push("brave".into());
            }
            if !cfg.search.searxng_url.is_empty() {
                v.push("searxng".into());
            }
            v
        }
    }
}
fn fetch_backend(backend: &str, query: &str, count: usize, cfg: &Config) -> Result<Vec<SearchResult>, String> {
    match backend {
        "ddg" => ddg_search(query, count, DDG_BASE),
        "bing" => bing_search(query, count, BING_BASE),
        "brave" => {
            if cfg.search.brave_key.is_empty() {
                return Err("未配置 brave_key（config.json: search.brave_key）".into());
            }
            brave_search(query, count, &cfg.search.brave_key, BRAVE_BASE)
        }
        "searxng" => {
            if cfg.search.searxng_url.is_empty() {
                return Err("未配置 searxng_url（config.json: search.searxng_url）".into());
            }
            searxng_search(query, count, &cfg.search.searxng_url, &cfg.search.searxng_key)
        }
        _ => Err(format!("未知后端: {backend}（auto/ddg/bing/brave/searxng）")),
    }
}
fn ddg_search(query: &str, count: usize, base: &str) -> Result<Vec<SearchResult>, String> {
    let url = format!("{}/html/?q={}", base.trim_end_matches('/'), urlencode(query));
    let body = http_get_text(&url, 15)?;
    let rs = parse_ddg_html(&body, count);
    if rs.is_empty() && body.contains("anomaly") {
        return Err("DuckDuckGo 拒绝访问（反爬/网络受限）；可换 backend=brave 或配置 search.searxng_url".into());
    }
    Ok(rs)
}
fn bing_search(query: &str, count: usize, base: &str) -> Result<Vec<SearchResult>, String> {
    let url = format!("{base}?q={}", urlencode(query));
    let body = http_get_text(&url, 15)?;
    let rs = parse_bing_html(&body, count);
    if rs.is_empty() && (body.contains("challenge") || body.contains("Please verify")) {
        return Err("Bing 拒绝访问（验证页）；可换 backend=brave 或配置 search.searxng_url".into());
    }
    Ok(rs)
}
fn parse_bing_html(html: &str, max: usize) -> Vec<SearchResult> {
    const MARKER: &str = "<li class=\"b_algo";
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while out.len() < max {
        let Some(rel) = html[cursor..].find(MARKER) else { break };
        let pos = cursor + rel;
        let next_anchor = html[pos + MARKER.len()..].find(MARKER).map(|j| pos + MARKER.len() + j);
        let end = next_anchor.unwrap_or(html.len()).min(pos + 8000);
        let seg = &html[pos..end];
        let (title, url) = match seg.find("<h2") {
            Some(h) => match seg[h..].find("<a ") {
                Some(a) => {
                    let apos = h + a;                        
                    let href = seg[apos..]
                        .find("href=\"")
                        .and_then(|j| {
                            let s = apos + j + 6;
                            seg[s..].find('"').map(|k| seg[s..s + k].to_string())
                        })
                        .unwrap_or_default();
                    let title = anchor_text(seg, apos).unwrap_or_default();
                    (title, href)
                }
                None => (String::new(), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let snippet = match seg.find("class=\"b_caption\"") {
            Some(c) => match seg[c..].find("<p") {
                Some(p) => p_text(&seg[c + p..]).unwrap_or_default(),
                None => String::new(),
            },
            None => String::new(),
        };
        if !title.is_empty() || !url.is_empty() {
            out.push(SearchResult { title, url, snippet });
        }
        cursor = pos + MARKER.len();
    }
    out
}
fn p_text(seg: &str) -> Option<String> {
    let after = seg.find('>')? + 1;
    let close = seg[after..].find("</p>")? + after;
    Some(strip_tags(&seg[after..close]))
}
fn brave_search(query: &str, count: usize, key: &str, base: &str) -> Result<Vec<SearchResult>, String> {
    let url = format!("{base}?q={}&count={count}", urlencode(query));
    let body = http_get_with_headers(&url, &[("X-Subscription-Token", key)], 15)?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("Brave 响应解析失败: {e}"))?;
    let mut out = Vec::new();
    if let Some(results) = v.get("web").and_then(|w| w.get("results")).and_then(|r| r.as_array()) {
        for r in results {
            let title = r.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let url = r.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
            let snippet = r.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
            if !title.is_empty() || !url.is_empty() {
                out.push(SearchResult { title, url, snippet });
            }
        }
    }
    Ok(out)
}
fn searxng_search(query: &str, count: usize, base: &str, key: &str) -> Result<Vec<SearchResult>, String> {
    let url = format!("{}/search?q={}&format=json", base.trim_end_matches('/'), urlencode(query));
    let headers: Vec<(&str, &str)> = if key.is_empty() { Vec::new() } else { vec![("X-API-Key", key)] };
    let body = http_get_with_headers(&url, &headers, 15)?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("SearXNG 响应解析失败: {e}"))?;
    let mut out = Vec::new();
    if let Some(results) = v.get("results").and_then(|r| r.as_array()) {
        for r in results.iter().take(count) {
            let title = r.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let url = r.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
            let snippet = r.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            if !title.is_empty() || !url.is_empty() {
                out.push(SearchResult { title, url, snippet });
            }
        }
    }
    Ok(out)
}
fn http_get_text(url: &str, timeout: u64) -> Result<String, String> {
    http_get_with_headers(url, &[], timeout)
}
fn http_get_with_headers(url: &str, headers: &[(&str, &str)], timeout: u64) -> Result<String, String> {
    let mut req = ureq::get(url)
        .set("User-Agent", UA)
        .set("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .set("Accept", "application/json, text/html, */*;q=0.8")
        .timeout(Duration::from_secs(timeout.min(60)));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        other => format!("{other}"),
    })?;
    let mut body = String::new();
    resp.into_reader()
        .take(2 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| format!("读取响应失败: {e}"))?;
    Ok(body)
}
fn parse_ddg_html(html: &str, max: usize) -> Vec<SearchResult> {
    const MARKER: &str = "class=\"result__a\"";
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while out.len() < max {
        let Some(rel) = html[cursor..].find(MARKER) else { break };
        let pos = cursor + rel;
        let href = html[pos..]
            .find("href=\"")
            .and_then(|j| {
                let s = pos + j + 6;                   
                html[s..].find('"').map(|k| &html[s..s + k])
            })
            .map(resolve_href)
            .unwrap_or_default();
        let title = anchor_text(html, pos).unwrap_or_default();
        let next_anchor = html[pos + MARKER.len()..].find(MARKER).map(|j| pos + MARKER.len() + j);
        let window_end = next_anchor.unwrap_or(html.len()).min(pos + 3000);
        let snippet = find_anchor_text(html, pos, window_end, "class=\"result__snippet\"");
        let disp_url = find_anchor_text(html, pos, window_end, "class=\"result__url\"");
        let url = if !href.is_empty() { href } else { disp_url };
        if !title.is_empty() || !url.is_empty() {
            out.push(SearchResult { title, url, snippet });
        }
        cursor = pos + MARKER.len();
    }
    out
}
fn find_anchor_text(hay: &str, start: usize, end: usize, marker: &str) -> String {
    let seg = &hay[start..end.min(hay.len())];
    match seg.find(marker) {
        Some(i) => anchor_text(hay, start + i).unwrap_or_default(),
        None => String::new(),
    }
}
fn anchor_text(hay: &str, pos: usize) -> Option<String> {
    let after = hay[pos..].find('>')? + pos + 1;
    let close = hay[after..].find("</a>")? + after;
    Some(strip_tags(&hay[after..close]))
}
fn resolve_href(href: &str) -> String {
    if let Some(i) = href.find("uddg=") {
        let enc = href[i + 5..].split('&').next().unwrap_or("");
        let dec = percent_decode(enc);
        if dec.starts_with("http://") || dec.starts_with("https://") {
            return dec;
        }
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    String::new()
}
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;              
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
fn strip_tags(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut in_tag = false;                         
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            in_tag = true;
            let mut name = String::new();
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric()) {
                name.push(chars[j].to_ascii_lowercase());           
                j += 1;
            }
            if is_block_tag(&name) {
                out.push('\n');
            }
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
        i += 1;
    }
    unescape_entities(out.trim())
}
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "ul" | "ol" | "table"
            | "tr" | "td" | "th" | "section" | "article" | "header" | "footer" | "blockquote"
            | "pre" | "br" | "hr"
    )
}
fn unescape_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&ensp;", " ")
        .replace("&#0183;", "·")
        .replace("&#x2F;", "/")
}
pub fn web_fetch(args: &Value) -> Result<String, String> {
    let mut urls = string_list(args, "urls");
    if urls.is_empty() {
        if let Some(url) = args.get("url").and_then(Value::as_str).map(str::trim).filter(|url| !url.is_empty()) {
            urls.push(url.to_string());
        }
    }
    urls.truncate(MAX_FETCH_URLS);
    if urls.is_empty() {
        return Err("缺少参数: url（也可传 urls 数组）".into());
    }
    let max_chars = args.get("max_chars").and_then(|m| m.as_u64()).unwrap_or(8000) as usize;
    let max_chars = max_chars.clamp(500, 50_000);
    if urls.len() == 1 {
        return web_fetch_one(&urls[0], max_chars);
    }
    let mut completed = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = urls
            .iter()
            .cloned()
            .map(|url| scope.spawn(move || {
                let result = web_fetch_one(&url, max_chars);
                (url, result)                       
            }))
            .collect();
        for handle in handles {
            if let Ok(result) = handle.join() {
                completed.push(result);
            }
        }
    });
    let mut out = format!("批量抓取（{} 个 URL）\n", completed.len());
    for (index, (url, result)) in completed.into_iter().enumerate() {
        out.push_str(&format!("\n===== {}. {} =====\n", index + 1, url));
        match result {
            Ok(text) => out.push_str(&text),
            Err(error) => out.push_str(&format!("抓取失败: {error}")),
        }
        out.push('\n');
    }
    Ok(out)
}
fn web_fetch_one(url: &str, max_chars: usize) -> Result<String, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("不支持的 URL 协议: {url}"));
    }
    let max_chars = max_chars.clamp(500, 50_000);
    let resp = ureq::get(url)
        .set("User-Agent", UA)
        .set("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .set("Accept", "text/html,application/xhtml+xml,application/json,*/*;q=0.8")
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            other => format!("请求失败: {other}"),
        })?;
    let status = resp.status();
    let ctype = resp.header("Content-Type").unwrap_or("").to_lowercase();
    let mut raw: Vec<u8> = Vec::new();
    resp.into_reader()
        .take(2 * 1024 * 1024)
        .read_to_end(&mut raw)
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let body = String::from_utf8_lossy(&raw).into_owned();
    let is_html = ctype.contains("html") || body.contains("<html") || body.contains("<!doctype");
    let (title, text) = if is_html {
        (html_title(&body), html_to_text(&body))
    } else {
        (String::new(), body.trim().to_string())
    };
    let total = text.chars().count();
    let shown: String = text.chars().take(max_chars).collect();
    let mut out = if title.is_empty() {
        format!("HTTP {status}（{total} 字符，显示 {}）\n", shown.chars().count())
    } else {
        format!("HTTP {status}｜标题: {title}\n（{total} 字符，显示 {}）\n", shown.chars().count())
    };
    out.push_str(&shown);
    if total > max_chars {
        out.push_str(&format!("\n…已截断（共 {total} 字符）；需要更多内容请调大 max_chars"));
    }
    Ok(out)
}
fn html_title(html: &str) -> String {
    match html.find("<title>") {
        Some(i) => match html[i + 7..].find("</title>") {
            Some(j) => strip_tags(&html[i + 7..i + 7 + j]),
            None => String::new(),
        },
        None => String::new(),
    }
}
pub fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["script", "style", "noscript", "title"] {
        let open_pat = format!("<{tag}");                    
        let close_pat = format!("</{tag}>");        
        let mut i = 0;
        while i < s.len() {
            let Some(rel) = s[i..].find(&open_pat) else { break };
            let open = i + rel;
            let Some(gt) = s[open..].find('>') else { break };
            let after_open = open + gt + 1;        
            match s[after_open..].find(&close_pat) {
                Some(rel2) => {
                    let end = after_open + rel2 + close_pat.len();
                    s.replace_range(open..end, "");
                    i = open;               
                }
                None => {
                    s.truncate(open);
                    break;
                }
            }
        }
    }
    while let Some(a) = s.find("<!--") {
        let Some(b) = s[a..].find("-->") else {
            s.truncate(a);
            break;
        };
        s.replace_range(a..a + b + 3, "");
    }
    for t in [
        "</p>", "</div>", "</h1>", "</h2>", "</h3>", "</h4>", "</h5>", "</h6>", "</li>", "</tr>",
        "</pre>", "</blockquote>", "</section>", "</article>", "</header>", "</footer>", "</ul>",
        "</ol>", "</table>", "<br>", "<br/>", "<br />", "<hr>",
    ] {
        s = s.replace(t, "\n");
    }
    let text = strip_tags(&s);
    let mut out = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;                  
            }
            out.push('\n');                 
        } else {
            blank = 0;                
            out.push_str(l);
            out.push('\n');
        }
    }
    out.trim().to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn mock_server(resp: String) -> String {
        use std::net::TcpListener;
        use std::io::Write;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }
    fn http_resp(body: &str, ctype: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }
    const SAMPLE_DDG: &str = r#"<html><body>
<div class="result results_links results_links_deep web-result ">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc">Rust <b>async</b> tutorial</a></h2>
<a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc">Learn async &amp; await in Rust</a>
<div class="result__extras__url"><a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc">example.com</a></div>
</div>
<div class="result results_links results_links_deep web-result ">
<h2 class="result__title"><a rel="nofollow" class="result__a" href="https://direct.example.org/page">Second <b>result</b> title</a></h2>
<a class="result__snippet" href="https://direct.example.org/page">Second snippet here</a>
<div class="result__extras__url"><a class="result__url" href="https://direct.example.org/page">direct.example.org</a></div>
</div>
</body></html>"#;
    #[test]
    fn parse_ddg_extracts_results() {
        let rs = parse_ddg_html(SAMPLE_DDG, 10);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].title, "Rust async tutorial");
        assert_eq!(rs[0].url, "https://example.com/rust");
        assert_eq!(rs[0].snippet, "Learn async & await in Rust");
        assert_eq!(rs[1].title, "Second result title");
        assert_eq!(rs[1].url, "https://direct.example.org/page");
        assert_eq!(rs[1].snippet, "Second snippet here");
        assert_eq!(parse_ddg_html(SAMPLE_DDG, 1).len(), 1);
    }
    #[test]
    fn ddg_search_hits_mock_server() {
        let base = mock_server(http_resp(SAMPLE_DDG, "text/html"));
        let rs = ddg_search("rust async", 10, &base).unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].title, "Rust async tutorial");
        assert_eq!(rs[0].url, "https://example.com/rust");
    }
    #[test]
    fn ddg_search_anomaly_detected() {
        let base = mock_server(http_resp("<html>anomaly-check</html>", "text/html"));
        let r = ddg_search("test", 10, &base);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("拒绝访问"));
    }
    const SAMPLE_BING: &str = r#"<html><body><ol id="b_results">
<li class="b_algo" data-id iid=SERP.1><h2 class=""><a target="_blank" href="https://rust-lang.github.io/async-book/">Introduction - <strong>Asynchronous</strong> Programming in <strong>Rust</strong></a></h2><div class="b_caption"><p class="b_lineclamp2">May 24, 2026&ensp;&#0183;&ensp;We don't assume any experience.</p></div></li>
<li class="b_algo b_algo_border" data-id iid=SERP.2><h2><a target="_blank" href="https://github.com/rustcn-org/async-book">GitHub - rustcn-org/<strong>async</strong>-<strong>book</strong></a></h2><div class="b_caption"><p class="b_lineclamp2">中文书名&lt;&lt;Rust 异步编程指南&gt;&gt;</p></div></li>
</ol></body></html>"#;
    #[test]
    fn parse_bing_extracts_results() {
        let rs = parse_bing_html(SAMPLE_BING, 10);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].title, "Introduction - Asynchronous Programming in Rust");
        assert_eq!(rs[0].url, "https://rust-lang.github.io/async-book/");
        assert_eq!(rs[0].snippet, "May 24, 2026 · We don't assume any experience.");
        assert_eq!(rs[1].title, "GitHub - rustcn-org/async-book");
        assert_eq!(rs[1].snippet, "中文书名<<Rust 异步编程指南>>");
        assert_eq!(parse_bing_html(SAMPLE_BING, 1).len(), 1);
    }
    #[test]
    fn bing_search_hits_mock_server() {
        let base = mock_server(http_resp(SAMPLE_BING, "text/html"));
        let rs = bing_search("rust async", 10, &base).unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://rust-lang.github.io/async-book/");
    }
    #[test]
    fn brave_search_parses_json() {
        let body = r#"{"web":{"results":[
            {"title":"Rust Book","url":"https://doc.rust-lang.org/book/","description":"The Rust Programming Language"},
            {"title":"Async Book","url":"https://book.async.rs/","description":"Async programming in Rust"}
        ]}}"#;
        let base = mock_server(http_resp(body, "application/json"));
        let rs = brave_search("rust", 10, "fake-key", &base).unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].url, "https://doc.rust-lang.org/book/");
        assert_eq!(rs[1].snippet, "Async programming in Rust");
    }
    #[test]
    fn searxng_search_parses_json() {
        let body = r#"{"results":[
            {"title":"A","url":"https://a.example/","content":"snippet A"},
            {"title":"B","url":"https://b.example/","content":"snippet B"}
        ]}"#;
        let base = mock_server(http_resp(body, "application/json"));
        let rs = searxng_search("q", 1, &base, "").unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].title, "A");
    }
    #[test]
    fn web_fetch_html_to_text() {
        let html = r#"<html><head><title>测试页面</title></head><body>
<script>var x = 1;</script>
<style>.a{color:red}</style>
<h1>标题一</h1>
<p>第一段 &amp; 内容</p>
<div>第二段<br>换行内容</div>
<!-- 注释不应出现 -->
<ul><li>条目1</li><li>条目2</li></ul>
</body></html>"#;
        let base = mock_server(http_resp(html, "text/html"));
        let out = web_fetch(&json!({"url": format!("{base}/page"), "max_chars": 5000})).unwrap();
        assert!(out.contains("标题: 测试页面"), "应提取 title: {out}");
        assert!(out.contains("标题一"));
        assert!(out.contains("第一段 & 内容"), "实体应解码");
        assert!(out.contains("第二段"));
        assert!(out.contains("换行内容"));
        assert!(out.contains("条目1"));
        assert!(!out.contains("var x"), "script 应被剥除");
        assert!(!out.contains("注释"), "注释应被剥除");
        assert!(out.contains("HTTP 200"));
    }
    #[test]
    fn web_fetch_max_chars_truncates() {
        let html = format!("<html><title>T</title><body>{}</body></html>", "字".repeat(2000));
        let base = mock_server(http_resp(&html, "text/html"));
        let out = web_fetch(&json!({"url": format!("{base}/big"), "max_chars": 500})).unwrap();
        assert!(out.contains("已截断"), "out: {out}");
        assert!(out.contains("共 2000 字符"), "out: {out}");
    }
    #[test]
    fn web_fetch_rejects_bad_protocol() {
        let r = web_fetch(&json!({"url": "file:///etc/passwd"}));
        assert!(r.is_err());
    }
    #[test]
    fn percent_decode_and_entities() {
        assert_eq!(percent_decode("https%3A%2F%2Fexample.com%2Frust"), "https://example.com/rust");
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(unescape_entities("a &amp; b &lt;c&gt; &quot;d&quot;"), "a & b <c> \"d\"");
    }
    #[test]
    fn resolve_href_handles_uddg_redirect() {
        assert_eq!(
            resolve_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fx&rut=abc"),
            "https://example.com/x"
        );
        assert_eq!(resolve_href("https://direct.example.org/page"), "https://direct.example.org/page");
        assert_eq!(resolve_href("/relative/path"), "");
    }
    #[test]
    fn web_search_validates_and_resolves() {
        let cfg = Config::default();
        assert!(web_search(&json!({"query": ""}), &cfg).is_err());
        assert!(web_search(&json!({}), &cfg).is_err());
        let err = web_search(&json!({"query": "x", "backend": "brave"}), &cfg).unwrap_err();
        assert!(err.contains("brave_key"), "err: {err}");
        assert_eq!(resolve_backends("auto", &cfg), vec!["ddg", "bing"]);
        assert_eq!(resolve_backends("ddg", &cfg), vec!["ddg"]);
        assert_eq!(resolve_backends("nope", &cfg), vec!["ddg", "bing"]);
    }
    #[test]
    fn canonical_url_removes_tracking_and_fragments() {
        assert_eq!(
            canonical_url("https://Example.com/page/?utm_source=x&id=7#part"),
            "https://Example.com/page?id=7"
        );
        assert_eq!(canonical_url("file:///etc/passwd"), "");
    }
    #[test]
    fn filters_and_quality_are_deterministic() {
        assert!(domain_allowed("https://docs.rust-lang.org/book/", &["rust-lang.org".into()], &[]));
        assert!(!domain_allowed("https://spam.example/x", &[], &["example".into()]));
        assert!(domain_quality("https://docs.rs/serde") > domain_quality("https://pinterest.com/x"));
    }
    #[test]
    fn research_generates_diverse_queries_but_respects_explicit_queries() {
        let generated = queries_from_args(&json!({"query":"Rust async"}), true).unwrap();
        assert_eq!(generated.len(), 3);
        assert!(generated[1].contains("official"));
        let explicit = queries_from_args(&json!({"queries":["one", "two"]}), true).unwrap();
        assert_eq!(explicit, vec!["one", "two"]);
    }
    #[test]
    fn web_fetch_batches_and_keeps_partial_failures() {
        let html = "<html><title>A</title><body>alpha</body></html>";
        let base = mock_server(http_resp(html, "text/html"));
        let out = web_fetch(&json!({
            "urls": [format!("{base}/ok"), "file:///etc/passwd"],
            "max_chars": 1000
        }))
        .unwrap();
        assert!(out.contains("批量抓取（2 个 URL）"));
        assert!(out.contains("标题: A"));
        assert!(out.contains("不支持的 URL 协议"));
    }
    #[test]
    fn html_to_text_compresses_blank_lines() {
        let out = html_to_text("<p>a</p>\n\n\n\n<p>b</p>");
        assert_eq!(out, "a\n\nb");
        assert_eq!(html_to_text("plain text"), "plain text");
    }
    #[test]
    #[ignore]
    fn live_web_ddg_and_fetch() {
        let cfg = Config::default();
        let out = web_search(&json!({"query": "rust async book", "count": 5}), &cfg).unwrap();
        eprintln!("web_search auto:\n{out}");
        assert!(out.contains("搜索 \"rust async book\""), "out: {out}");
        assert!(!out.contains("所有搜索后端均失败"), "out: {out}");
        let out2 = web_fetch(&json!({"url": "https://example.com", "max_chars": 2000})).unwrap();
        eprintln!("web_fetch example.com:\n{out2}");
        assert!(out2.contains("HTTP 200"));
    }
}
