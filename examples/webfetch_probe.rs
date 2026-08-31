use serde_json::json;

fn main() {
    let url = std::env::args().nth(1).expect("用法: webfetch_probe <url> [max_chars]");
    let max_chars: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3000);
    let t0 = std::time::Instant::now();
    match yjlcoder::web::web_fetch(&json!({"url": url, "max_chars": max_chars})) {
        Ok(out) => println!("web_fetch {url} => 耗时 {:?}\n{}", t0.elapsed(), &out[..out.len().min(1200)]),
        Err(e) => println!("web_fetch {url} => 失败: {e}"),
    }
}
