use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Style {
    Plain,
    Bold,
    Italic,
    Code,
    Strike,
    Link,
    Dim,
}
#[derive(Debug, Clone)]
pub struct Seg {
    pub style: Style,
    pub text: String,
}
impl Seg {
    pub fn new(style: Style, text: impl Into<String>) -> Self {
        Seg { style, text: text.into() }
    }
}
pub fn inline(s: &str) -> Vec<Seg> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<Seg> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_close(&chars, i + 2, &['*', '*']) {
                flush_plain(&mut out, &mut plain);
                out.push(Seg::new(Style::Bold, collect(&chars, i + 2, end)));
                i = end + 2;
                continue;
            }
        }
        if c == '*' && chars.get(i + 1) != Some(&'*') {
            if let Some(end) = find_close(&chars, i + 1, &['*']) {
                flush_plain(&mut out, &mut plain);
                out.push(Seg::new(Style::Italic, collect(&chars, i + 1, end)));
                i = end + 1;
                continue;
            }
        }
        if c == '`' {
            if let Some(end) = find_close(&chars, i + 1, &['`']) {
                flush_plain(&mut out, &mut plain);
                out.push(Seg::new(Style::Code, collect(&chars, i + 1, end)));
                i = end + 1;
                continue;
            }
        }
        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_close(&chars, i + 2, &['~', '~']) {
                flush_plain(&mut out, &mut plain);
                out.push(Seg::new(Style::Strike, collect(&chars, i + 2, end)));
                i = end + 2;
                continue;
            }
        }
        if c == '[' {
            if let Some(bracket_end) = find_close(&chars, i + 1, &[']']) {
                let after = bracket_end + 1;
                if chars.get(after) == Some(&'(') {
                    if let Some(end) = find_close(&chars, after + 1, &[')']) {
                        flush_plain(&mut out, &mut plain);
                        let text = collect(&chars, i + 1, bracket_end);
                        let url = collect(&chars, after + 1, end);
                        out.push(Seg::new(Style::Link, text));
                        out.push(Seg::new(Style::Dim, format!(" ({url})")));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        plain.push(c);
        i += 1;
    }
    flush_plain(&mut out, &mut plain);
    out
}
fn find_close(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    let ml = marker.len();
    let mut i = from;
    while i + ml <= chars.len() {
        if &chars[i..i + ml] == marker {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn collect(chars: &[char], from: usize, to: usize) -> String {
    chars[from..to].iter().collect()
}
fn flush_plain(out: &mut Vec<Seg>, plain: &mut String) {
    if !plain.is_empty() {
        out.push(Seg::new(Style::Plain, std::mem::take(plain)));
    }
}
pub fn block_parts(text: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    let mut plain = String::new();
    let mut code = String::new();
    let mut in_code = false;
    let mut md_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                out.push((true, std::mem::take(&mut code)));
                in_code = false;
            } else if md_fence {
                md_fence = false;
            } else {
                if !plain.is_empty() {
                    out.push((false, std::mem::take(&mut plain)));
                }
                let lang = trimmed.trim_start_matches('`').trim();
                if lang == "markdown" || lang == "md" {
                    md_fence = true;
                } else {
                    in_code = true;
                }
            }
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
        } else {
            plain.push_str(line);
            plain.push('\n');
        }
    }
    if in_code {
        out.push((true, std::mem::take(&mut code)));
    }
    if !plain.is_empty() || out.is_empty() {
        out.push((false, plain));
    }
    out
}
pub fn block_prefix(line: &str) -> (Option<Style>, &'static str, &str) {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
        return (Some(Style::Bold), "", &trimmed[hashes + 1..]);
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return (None, "│ ", rest);
    }
    (None, "", line)
}
#[derive(Debug, Clone)]
pub struct Table {
    pub rows: Vec<Vec<String>>,
}
pub fn split_cells(line: &str) -> Vec<String> {
    const PIPE_ESC: char = '\u{1F}';
    let trimmed = line.trim().trim_matches('|');
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            cur.push(PIPE_ESC);
        } else if c == '|' {
            cells.push(std::mem::take(&mut cur).trim().to_string());
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells.into_iter().map(|c| c.replace(PIPE_ESC, "|")).collect()
}
pub fn is_sep_row(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.contains('|') {
        return false;
    }
    trimmed.trim_matches('|').split('|').all(|cell| {
        let cell = cell.trim().trim_matches(':').trim();
        !cell.is_empty() && cell.chars().all(|c| c == '-')
    })
}
pub fn parse_table(lines: &[&str]) -> Option<(Table, usize)> {
    if lines.len() < 2 {
        return None;
    }
    let head = lines[0].trim();
    if !head.starts_with('|') || !head.contains('|') || !is_sep_row(lines[1]) {
        return None;
    }
    let mut rows = vec![split_cells(head)];
    let mut n = 2;
    while n < lines.len() {
        let l = lines[n].trim();
        if l.starts_with('|') && l.contains('|') {
            rows.push(split_cells(l));
            n += 1;
        } else {
            break;
        }
    }
    Some((Table { rows }, n))
}
fn compute_widths(t: &Table, max_width: usize) -> Vec<usize> {
    let cols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for r in &t.rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(UnicodeWidthStr::width(c.as_str())).min(48);
        }
    }
    let total = widths.iter().sum::<usize>() + 3 * cols + 1;
    let mut overshoot = total.saturating_sub(max_width);
    while overshoot > 0 {
        let mut reduced = false;
        for w in widths.iter_mut() {
            if overshoot == 0 {
                break;
            }
            if *w > 3 {
                *w -= 1;
                overshoot -= 1;
                reduced = true;
            }
        }
        if !reduced {
            break;
        }
    }
    widths
}
fn fit(s: &str, w: usize) -> String {
    if UnicodeWidthStr::width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut cur = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        if cur + cw > w {
            break;
        }
        out.push(ch);
        cur += cw;
    }
    if UnicodeWidthStr::width(out.as_str()) < w {
        out.push('…');
    }
    out
}
fn table_row_line(cells: &[String], widths: &[usize]) -> String {
    let mut out = String::from("│");
    for (i, w) in widths.iter().enumerate() {
        let cell = cells.get(i).map(|c| fit(c, *w)).unwrap_or_default();
        out.push(' ');
        out.push_str(&cell);
        out.push_str(&" ".repeat(w.saturating_sub(UnicodeWidthStr::width(cell.as_str()))));
        out.push_str(" │");
    }
    out
}
pub fn render_table(t: &Table, max_width: usize) -> Vec<String> {
    let widths = compute_widths(t, max_width);
    let rows: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|r| {
            let mut padded = r.clone();
            padded.resize(widths.len(), String::new());
            padded
        })
        .collect();
    let sep = |left: &str, mid: &str, right: &str| {
        let mut s = String::from(left);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 < widths.len() { mid } else { right });
        }
        s
    };
    let mut out = vec![sep("┌", "┬", "┐")];
    out.push(table_row_line(&rows[0], &widths));
    out.push(sep("├", "┼", "┤"));
    for r in &rows[1..] {
        out.push(table_row_line(r, &widths));
    }
    out.push(sep("└", "┴", "┘"));
    out
}
const RESET: &str = "\x1b[0m";
fn style_ansi(style: Style) -> &'static str {
    match style {
        Style::Plain => "",
        Style::Bold => "\x1b[1m",
        Style::Italic => "\x1b[3m",
        Style::Code => "\x1b[48;5;234m",
        Style::Strike => "\x1b[9m",
        Style::Link => "\x1b[38;5;75m",
        Style::Dim => "\x1b[2m",
    }
}
pub fn render_segs(segs: &[Seg], base: &str) -> String {
    let mut out = String::new();
    let mut cur: Option<&'static str> = None;
    for seg in segs {
        let ansi = style_ansi(seg.style);
        if cur != Some(ansi) {
            out.push_str(RESET);
            out.push_str(base);
            out.push_str(ansi);
            cur = Some(ansi);
        }
        out.push_str(&seg.text);
    }
    out
}
pub fn wrap_segs(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    let mut w = 0usize;
    for seg in segs {
        for ch in seg.text.chars() {
            if ch == '\n' {
                out.push(std::mem::take(&mut cur));
                w = 0;
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            if w + cw > width && w > 0 {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(Seg::new(seg.style, ch.to_string()));
            w += cw;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    fn styles(segs: &[Seg]) -> Vec<Style> {
        segs.iter().map(|s| s.style).collect()
    }
    #[test]
    fn inline_bold_italic_code_strike_link() {
        let segs = inline("**粗体** 和 `code` 与 ~~删除~~ [链接](https://x.com)");
        let st = styles(&segs);
        assert_eq!(st, vec![
            Style::Bold, Style::Plain, Style::Code, Style::Plain,
            Style::Strike, Style::Plain, Style::Link, Style::Dim,
        ]);
        assert_eq!(segs[0].text, "粗体");
        assert_eq!(segs[2].text, "code");
        assert_eq!(segs[4].text, "删除");
        assert_eq!(segs[6].text, "链接");
        assert!(segs[7].text.contains("https://x.com"));
    }
    #[test]
    fn inline_no_markers_single_plain() {
        let segs = inline("普通文本 abc 下划线_不解析");
        assert_eq!(styles(&segs), vec![Style::Plain]);
        assert_eq!(segs[0].text, "普通文本 abc 下划线_不解析");
    }
    #[test]
    fn inline_single_asterisk_is_italic() {
        let segs = inline("a*b*c");
        assert_eq!(styles(&segs), vec![Style::Plain, Style::Italic, Style::Plain]);
        assert_eq!(segs[1].text, "b");
    }
    #[test]
    fn inline_unclosed_marker_stays_plain() {
        let segs = inline("未闭合 **粗体");
        assert_eq!(styles(&segs), vec![Style::Plain]);
        assert_eq!(segs[0].text, "未闭合 **粗体");
    }
    #[test]
    fn render_segs_switches_styles() {
        let segs = vec![
            Seg::new(Style::Plain, "a"),
            Seg::new(Style::Bold, "b"),
            Seg::new(Style::Code, "c"),
        ];
        let out = render_segs(&segs, "\x1b[37m");
        assert!(out.starts_with("\x1b[0m\x1b[37m"), "out: {out:?}");
        assert!(out.contains("\x1b[0m\x1b[37m\x1b[1m"), "粗体切换: {out:?}");
        assert!(out.contains("\x1b[0m\x1b[37m\x1b[48;5;234m"), "代码底色: {out:?}");
        assert!(out.ends_with('c'));
    }
    #[test]
    fn wrap_segs_preserves_style_across_lines() {
        let segs = vec![Seg::new(Style::Bold, "你好世界abcdef")];
        let lines = wrap_segs(&segs, 6);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.iter().all(|s| s.style == Style::Bold)));
        let first: String = lines[0].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(first, "你好世");
        let rest: String = lines[1].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rest, "界abcd");
    }
    #[test]
    fn wrap_segs_respects_newlines() {
        let segs = vec![Seg::new(Style::Plain, "a\nb")];
        let lines = wrap_segs(&segs, 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].text, "a");
        assert_eq!(lines[1][0].text, "b");
    }
    #[test]
    fn block_parts_splits_code_fence() {
        let parts = block_parts("开头\n```rust\nfn main() {}\n```\n结尾");
        assert_eq!(parts.len(), 3);
        assert!(!parts[0].0 && parts[0].1.contains("开头"));
        assert!(parts[1].0 && parts[1].1.contains("fn main()"));
        assert!(!parts[2].0 && parts[2].1.contains("结尾"));
    }
    #[test]
    fn block_parts_unclosed_fence() {
        let parts = block_parts("```python\nx = 1\n");
        assert_eq!(parts.len(), 1);
        assert!(parts[0].0);
        assert!(parts[0].1.contains("x = 1"));
    }
    #[test]
    fn block_parts_markdown_fence_treated_as_plain() {
        let src = "```markdown\n| a | b |\n|---|---|\n| 1 | 2 |\n```";
        let parts = block_parts(src);
        assert_eq!(parts.len(), 1, "markdown 围栏不产生代码块");
        assert!(!parts[0].0, "markdown 围栏内容按普通文本");
        assert!(parts[0].1.contains("| a | b |"));
        let parts = block_parts("```md\n# 标题\n```\n结尾");
        assert_eq!(parts.len(), 1);
        assert!(!parts[0].0, "md 围栏也是普通文本");
        assert!(parts[0].1.contains("# 标题"));
        assert!(parts[0].1.contains("结尾"), "围栏外后续文本在同一段: {}", parts[0].1);
    }
    #[test]
    fn block_parts_markdown_fence_inside_and_code_after() {
        let src = "```markdown\n| x |\n|---|\n| y |\n```\n```rust\nfn main() {}\n```";
        let parts = block_parts(src);
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].0, "markdown 围栏是普通文本");
        assert!(parts[1].0 && parts[1].1.contains("fn main()"), "后续 rust 围栏仍是代码块");
    }
    #[test]
    fn block_prefix_header_and_quote() {
        let (st, pre, rest) = block_prefix("### 安装说明");
        assert_eq!(st, Some(Style::Bold));
        assert_eq!(pre, "");
        assert_eq!(rest, "安装说明");
        let (st, pre, rest) = block_prefix("> 引用一段");
        assert_eq!(st, None);
        assert_eq!(pre, "│ ");
        assert_eq!(rest, "引用一段");
        let (st, pre, rest) = block_prefix("- 列表项");
        assert_eq!(st, None);
        assert_eq!(pre, "");
        assert_eq!(rest, "- 列表项");
    }
    #[test]
    fn table_parse_and_render() {
        let lines = ["| 项目 | 状态 |", "|---|---|", "| Rust | 完成 |", "| 发布 | 进行中 |", "结束文字"];
        let (t, n) = parse_table(&lines).unwrap();
        assert_eq!(n, 4, "表格消费 4 行，剩余文本不消费");
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0], vec!["项目", "状态"]);
        assert_eq!(t.rows[2], vec!["发布", "进行中"]);
        let rendered = render_table(&t, 40);
        assert_eq!(rendered[0], "┌──────┬────────┐");
        assert_eq!(rendered[1], "│ 项目 │ 状态   │");
        assert_eq!(rendered[2], "├──────┼────────┤");
        assert_eq!(rendered[3], "│ Rust │ 完成   │");
        assert_eq!(rendered[5], "└──────┴────────┘");
    }
    #[test]
    fn table_no_separator_returns_none() {
        assert!(parse_table(&["| a | b |"]).is_none(), "单行非表格");
        assert!(parse_table(&["| a | b |", "普通文字"]).is_none(), "无分隔行");
        assert!(parse_table(&["普通 | 文字", "|---|---|"]).is_none(), "表头不以 | 开头");
        assert!(parse_table(&["| a |", "|---|", "x"]).is_some(), "数据行仅一行也有效");
    }
    #[test]
    fn table_escaped_pipe_and_uneven_rows() {
        let (t, _) = parse_table(&["| 名字 | 值 |", "|---|---|", "| a\\|b | x |", "| 单列 |"]).unwrap();
        assert_eq!(t.rows[1][0], "a|b", "转义管道符还原");
        assert_eq!(t.rows[2], vec!["单列"]);
        let rendered = render_table(&t, 40);
        assert_eq!(rendered[3], "│ a|b  │ x  │", "数据行渲染: {}", rendered[3]);
        assert_eq!(rendered[4], "│ 单列 │    │", "缺列自动补空: {}", rendered[4]);
    }
    #[test]
    fn table_fits_width_truncates() {
        let (t, _) = parse_table(&["| 名字 |", "|---|", "| 超级无敌长的名字在这里显示不下 |"]).unwrap();
        for r in render_table(&t, 20) {
            assert!(UnicodeWidthStr::width(r.as_str()) <= 20, "表格行超宽: {r}");
        }
    }
}
