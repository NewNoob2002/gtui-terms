//! Running external commands (flash tools, gdb, coredump dumpers) from the TUI.

use std::process::Command;

pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

/// Output is meant for the scrollback, so it is capped.
const MAX_OUTPUT: usize = 64 * 1024;

#[cfg(windows)]
const SHELL: (&str, &str) = ("cmd", "/C");
#[cfg(not(windows))]
const SHELL: (&str, &str) = ("sh", "-c");

pub fn run(command: &str) -> Outcome {
    let (shell, flag) = SHELL;
    match Command::new(shell).arg(flag).arg(command).output() {
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
                text.truncate(MAX_OUTPUT);
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
    fn reports_failure_status() {
        let outcome = run("exit 3");
        assert!(!outcome.ok);
    }
}
