//! Running external commands (flash tools, gdb, coredump dumpers) from the TUI.

use std::process::Command;

pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

/// Output is meant for the scrollback, so it is capped.
const MAX_OUTPUT: usize = 64 * 1024;

pub fn run(command: &str) -> Outcome {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let mut process = Command::new(shell);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process
            .args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{command}\""));
    }
    #[cfg(not(windows))]
    process.args(["-c", command]);
    match process.output() {
        Ok(output) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            if text.len() > MAX_OUTPUT {
                let mut end = MAX_OUTPUT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            Outcome {
                ok: output.status.success(),
                text,
            }
        }
        Err(err) => Outcome {
            ok: false,
            text: format!("cannot start {shell}: {err}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_and_reports_success() {
        let outcome = run("echo hook-ok");
        assert!(outcome.ok);
        assert!(outcome.text.contains("hook-ok"), "{:?}", outcome.text);
    }

    #[test]
    fn unicode_output_limit_preserves_completion_and_valid_text() {
        let outcome = run(
            "python -c \"import sys; sys.stdout.buffer.write(b'a' * 65535 + bytes([0xc3, 0xa9])); sys.exit(3)\"",
        );
        assert!(!outcome.ok);
        assert_eq!(outcome.text.len(), MAX_OUTPUT - 1);
        assert!(outcome.text.bytes().all(|byte| byte == b'a'));
    }

    #[test]
    fn reports_failure_status() {
        let outcome = run("exit 3");
        assert!(!outcome.ok);
    }
}
