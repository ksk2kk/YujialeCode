                         
   
                                         
                                                
                                        
   
                                                             
                                                
                                                            
   
         
   
           
                                                              
                                                           
                                                                          
                                                                            
                                                                          
                                                     

use std::sync::atomic::AtomicBool;

use crate::llm::{ChatRequest, Llm, Msg};

pub const SUMMARIZATION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.";

pub const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

                                                       
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

                                               
const APPROX_BYTES_PER_TOKEN: usize = 4;

                                            
pub fn approx_token_count(text: &str) -> usize {
                                                 
    let len = text.len();
    len.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

                                          
pub fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

                             
pub fn approx_total_tokens(messages: &[Msg]) -> usize {
    messages.iter().map(|m| approx_token_count(&m.content)).sum()
}

                                       
pub fn truncate_middle_with_token_budget(s: &str, max_tokens: usize) -> (String, Option<u64>) {
    if s.is_empty() {
        return (String::new(), None);
    }
    if max_tokens > 0 && s.len() <= approx_bytes_for_tokens(max_tokens) {
        return (s.to_string(), None);
    }
                                                   
    let truncated = truncate_with_byte_estimate(s, approx_bytes_for_tokens(max_tokens), true);
    let total_tokens = u64::try_from(approx_token_count(s)).unwrap_or(u64::MAX);
    if truncated == s {
        (truncated, None)
    } else {
        (truncated, Some(total_tokens))
    }
}

fn truncate_with_byte_estimate(s: &str, max_bytes: usize, use_tokens: bool) -> String {
    if s.is_empty() {
        return String::new();
    }
    let total_chars = s.chars().count();
    if max_bytes == 0 {
        return format_truncation_marker(use_tokens, removed_units(use_tokens, s.len(), total_chars));
    }
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let total_bytes = s.len();
    let (left_budget, right_budget) = split_budget(max_bytes);
    let (removed_chars, left, right) = split_string(s, left_budget, right_budget);
    let marker = format_truncation_marker(
        use_tokens,
        removed_units(use_tokens, total_bytes.saturating_sub(max_bytes), removed_chars),
    );
    assemble_truncated_output(left, right, &marker)
}

                                          
                                
   
        
                         
                                  
                            
   
        
                                       
                                                      
   
                   
                                                 
                                                            
                   
                                                         
                                           
                                       
                                                        
                                 
                                                                    
                         
fn split_string(s: &str, beginning_bytes: usize, end_bytes: usize) -> (usize, &str, &str) {
    if s.is_empty() {
        return (0, "", "");
    }
    let len = s.len();
    let tail_start_target = len.saturating_sub(end_bytes);
    let mut prefix_end = 0usize;
    let mut suffix_start = len;
    let mut removed_chars = 0usize;
    let mut suffix_started = false;
                                                    
    for (idx, ch) in s.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= beginning_bytes {
            prefix_end = char_end;
            continue;
        }
        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }
        removed_chars = removed_chars.saturating_add(1);
    }
    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }
    let before = &s[..prefix_end];
    let after = &s[suffix_start..];
    (removed_chars, before, after)
}

fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

fn format_truncation_marker(use_tokens: bool, removed_count: u64) -> String {
    if use_tokens {
        format!("…{removed_count} tokens truncated…")
    } else {
        format!("…{removed_count} chars truncated…")
    }
}

fn removed_units(use_tokens: bool, removed_bytes: usize, removed_chars: usize) -> u64 {
    if use_tokens {
        approx_tokens_from_byte_count(removed_bytes)
    } else {
        u64::try_from(removed_chars).unwrap_or(u64::MAX)
    }
}

fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    let b = bytes as u64;
    b.saturating_add((APPROX_BYTES_PER_TOKEN as u64).saturating_sub(1)) / (APPROX_BYTES_PER_TOKEN as u64)
}

fn assemble_truncated_output(prefix: &str, suffix: &str, marker: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + marker.len() + suffix.len() + 1);
    out.push_str(prefix);
    out.push_str(marker);
    out.push_str(suffix);
    out
}

                                                              
pub fn formatted_truncate_text(content: &str, max_tokens: usize) -> String {
    if approx_token_count(content) <= max_tokens {
        return content.to_string();
    }
    let original_token_count = approx_token_count(content);
    let total_lines = content.lines().count();
    let (result, _) = truncate_middle_with_token_budget(content, max_tokens);
    format!(
        "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n提示：中间内容已被省略，不要据此判断文件完整内容；如需完整内容，缩小读取范围分页读取（readline 用 start/end）或先用 grep 定位目标行。\n\n{result}"
    )
}

                                           
pub fn is_summary_message(message: &str) -> bool {
    message.starts_with(&format!("{SUMMARY_PREFIX}\n"))
}

                                                                        
pub fn collect_user_messages(messages: &[Msg]) -> Vec<Msg> {
    messages
        .iter()
        .filter(|m| m.role == "user" && !is_summary_message(&m.content))
        .cloned()
        .collect()
}

                                          
                                           
                                   
   
        
                                                                    
                                              
                                            
   
        
                                                 
                        
fn build_compacted_history_with_limit(
    user_messages: &[Msg],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<Msg> {
                                             
    let mut history: Vec<Msg> = Vec::new();
    let mut selected: Vec<Msg> = Vec::new();
    if max_tokens > 0 {
        let mut remaining = max_tokens;
        for message in user_messages.iter().rev() {
            if remaining == 0 {
                break;
            }
            let tokens = approx_token_count(&message.content);
            if tokens <= remaining {
                selected.push(message.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                let (truncated, _) = truncate_middle_with_token_budget(&message.content, remaining);
                                                                
                                                   
                selected.push(Msg {
                    content: truncated,
                    ..message.clone()
                });
                break;                             
            }
        }
        selected.reverse();
    }
    history.extend(selected);

    let summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };
    history.push(Msg::new("user", summary_text));
    history
}

pub fn build_compacted_history(user_messages: &[Msg], summary_text: &str) -> Vec<Msg> {
    build_compacted_history_with_limit(user_messages, summary_text, COMPACT_USER_MESSAGE_MAX_TOKENS)
}

                                                             
                                                  
                              
                                                 
                                                        
                                   
pub fn compact(
    llm: &Llm,
    messages: &[Msg],
    cancel: &AtomicBool,
) -> Result<Vec<Msg>, String> {
    compact_inner(llm, messages, cancel, None)
}

                                          
                                         
pub fn compact_to_limit(
    llm: &Llm,
    messages: &[Msg],
    cancel: &AtomicBool,
    max_history_tokens: usize,
) -> Result<Vec<Msg>, String> {
    compact_inner(llm, messages, cancel, Some(max_history_tokens))
}

fn compact_inner(
    llm: &Llm,
    messages: &[Msg],
    cancel: &AtomicBool,
    max_history_tokens: Option<usize>,
) -> Result<Vec<Msg>, String> {
    let mut work = messages.to_vec();
    let mut retries = 0;
    loop {
        let mut req_msgs = work.clone();
        req_msgs.push(Msg::new("user", SUMMARIZATION_PROMPT));
                                        
                                                     
                                                       
                                        
        let summary_tokens = max_history_tokens
            .map(|limit| (limit / 2).clamp(128, 1024))
            .unwrap_or(1024);
        let req = ChatRequest {
            messages: req_msgs,
            tools: None,
            max_tokens: Some(summary_tokens),
            stream: false,
        };
        let result = llm.stream(&req, cancel, |_| {});
        match result {
            Ok(r) => {
                                                                    
                let mut summary = r.text;
                if summary.trim().is_empty() {
                    summary = "(no summary available)".to_string();
                }
                let summary_text = format!("{SUMMARY_PREFIX}\n{}", summary.trim());
                let user_msgs = collect_user_messages(&work);
                let mut new_history = if let Some(limit) = max_history_tokens {
                    let user_budget = limit.saturating_sub(approx_token_count(&summary_text));
                    let mut history =
                        build_compacted_history_with_limit(&user_msgs, &summary_text, user_budget);
                                                     
                                             
                    while history.len() > 1 && approx_total_tokens(&history) > limit {
                        history.remove(0);
                    }
                    history
                } else {
                    build_compacted_history(&user_msgs, &summary_text)
                };
                                  
                if let Some(last) = new_history.last_mut() {
                    last.content = summary_text.clone();
                }
                return Ok(new_history);
            }
            Err(e) => {
                if e.contains("context") && e.contains("length") && !work.is_empty() {
                                                               
                                                   
                                                    
                                                    
                                        
                    work.remove(0);
                    retries += 1;
                    if retries > 16 {
                        return Err(format!("压缩失败（上下文超限且重试耗尽）: {e}"));
                    }
                    continue;
                }
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_bytes_over_four() {
                                              
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
                    
        assert_eq!(approx_token_count("你好"), 2);
    }

    #[test]
    fn truncate_preserves_head_and_tail() {
        let s = "A".repeat(100) + "MIDDLE" + &"B".repeat(100);
        let (t, _) = truncate_middle_with_token_budget(&s, 20);           
        assert!(t.contains("truncated"));
        assert!(t.starts_with("AAAA"));
        assert!(t.ends_with("BBBB"));
    }

    #[test]
    fn build_history_selects_recent_and_appends_summary() {
        let mut msgs = Vec::new();
        for i in 0..10 {
            msgs.push(Msg::new("user", format!("旧消息{i}")));
        }
                                             
        let h = build_compacted_history_with_limit(&msgs, "摘要", 9);
        assert_eq!(h.len(), 4);
        assert_eq!(h[0].content, "旧消息7");
        assert_eq!(h[3].content, "摘要");
        assert!(h.last().unwrap().role == "user");
    }

    #[test]
    fn summary_detection() {
        assert!(is_summary_message(&format!("{SUMMARY_PREFIX}\n内容")));
        assert!(!is_summary_message("普通"));
    }

    #[test]
    fn collect_skips_prior_summaries() {
        let msgs = vec![
            Msg::new("user", format!("{SUMMARY_PREFIX}\n旧摘要")),
            Msg::new("user", "真实消息"),
            Msg::new("assistant", "回复"),
        ];
        let u = collect_user_messages(&msgs);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].content, "真实消息");
    }

    #[test]
    fn formatted_truncate_annotates() {
        let long = "x".repeat(4000);
        let t = formatted_truncate_text(&long, 100);
        assert!(t.contains("Warning: truncated output"));
    }

    #[test]
    fn compact_to_limit_respects_history_budget() {
        let llm = Llm::mock();
        let cancel = AtomicBool::new(false);
        let messages: Vec<Msg> = (0..30)
            .flat_map(|i| {
                [
                    Msg::new("user", format!("{i}:{}", "x".repeat(300))),
                    Msg::new("assistant", "y".repeat(100)),
                ]
            })
            .collect();
        let compacted = compact_to_limit(&llm, &messages, &cancel, 600).unwrap();
        assert!(approx_total_tokens(&compacted) <= 600);
        assert!(is_summary_message(&compacted.last().unwrap().content));
    }
}
