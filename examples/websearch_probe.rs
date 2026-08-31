use serde_json::json;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut repeat = 1usize;
    if let Some(pos) = args.iter().position(|a| a == "--repeat") {
        if let Some(n) = args.get(pos + 1).and_then(|s| s.parse().ok()) {
            repeat = n;
        }
        args.drain(pos..(pos + 2).min(args.len()));
    }
    let query = args.first().cloned().unwrap_or_else(|| "rust tutorial".into());
    let backend = args.get(1).cloned().unwrap_or_else(|| "auto".into());
    let cfg = yjlcoder::config::Config::default();
    let mut ok = 0usize;
    for round in 0..repeat {
        let t0 = std::time::Instant::now();
        match yjlcoder::web::web_search(
            &json!({"query": query, "backend": backend, "count": 8}),
            &cfg,
        ) {
            Ok(out) => {
                ok += 1;
                if round == 0 || std::env::var("PROBE_VERBOSE").is_ok() {
                    println!("[{backend}] {query} => 耗时 {:?}\n{}", t0.elapsed(), out);
                } else {
                    println!("[{backend}] 第{}轮 成功 耗时 {:?}", round + 1, t0.elapsed());
                }
            }
            Err(e) => println!("[{backend}] 第{}轮 失败 耗时 {:?}: {e}", round + 1, t0.elapsed()),
        }
    }
    println!("成功率: {ok}/{repeat}");
}
