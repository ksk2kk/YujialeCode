












use base64::Engine as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Stdio};

const OSC52_MAX_RAW_BYTES: usize = 100_000;


pub struct ClipboardLease {
    _clipboard: arboard::Clipboard,
}


pub fn copy_to_clipboard(text: &str) -> Result<Option<ClipboardLease>, String> {
    if text.is_empty() {
        return Err("没有可复制的文本".into());
    }

    if is_ssh() {
        if in_tmux() && copy_via_tmux(text) {
            return Ok(None);
        }
        write_osc52(text)?;
        return Ok(None);
    }

    match arboard::Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(text.to_owned()) {
            Ok(()) => return Ok(Some(ClipboardLease { _clipboard: clipboard })),
            Err(arboard_error) => {
                
                
                if let Err(osc_error) = write_osc52(text) {
                    return Err(format!("系统剪贴板失败: {arboard_error}；OSC52 失败: {osc_error}"));
                }
                return Ok(None);
            }
        },
        Err(arboard_error) => {
            if let Err(osc_error) = write_osc52(text) {
                return Err(format!("系统剪贴板不可用: {arboard_error}；OSC52 失败: {osc_error}"));
            }
        }
    }
    Ok(None)
}

fn is_ssh() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some()
}



fn copy_via_tmux(text: &str) -> bool {
    let Ok(mut child) = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut input| input.write_all(text.as_bytes()).is_ok());
    wrote && child.wait().is_ok_and(|status| status.success())
}

fn clipped_at_char_boundary(text: &str) -> &str {
    let mut end = text.len().min(OSC52_MAX_RAW_BYTES);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn osc52_sequence(text: &str, tmux_wrapped: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(clipped_at_char_boundary(text));
    if tmux_wrapped {
        
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    }
}

fn write_osc52(text: &str) -> Result<(), String> {
    let sequence = osc52_sequence(text, in_tmux());
    
    
    if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
        tty.write_all(sequence.as_bytes())
            .and_then(|_| tty.flush())
            .map_err(|e| format!("写入控制终端失败: {e}"))?;
        return Ok(());
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|_| stdout.flush())
        .map_err(|e| format!("写入终端失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_uses_standard_base64() {
        assert_eq!(osc52_sequence("YJL", false), "\x1b]52;c;WUpM\x07");
    }

    #[test]
    fn utf8_limit_never_splits_character() {
        let text = "你".repeat(OSC52_MAX_RAW_BYTES / 3 + 2);
        let clipped = clipped_at_char_boundary(&text);
        assert!(clipped.len() <= OSC52_MAX_RAW_BYTES);
        assert!(std::str::from_utf8(clipped.as_bytes()).is_ok());
    }

    #[test]
    fn tmux_sequence_has_dcs_passthrough_wrapper() {
        let sequence = osc52_sequence("x", true);
        assert!(sequence.starts_with("\x1bPtmux;\x1b\x1b]52;c;"));
        assert!(sequence.ends_with("\x07\x1b\\"));
    }
}
