//! A warm PowerShell 7 session. Spawning `pwsh` costs 300–600 ms; keeping one process
//! alive and feeding it commands over stdin costs only the command itself.
//!
//! Protocol: the child runs a tiny read-eval loop. Each command is sent as one line
//! `<id> <base64(utf-16le script)>`; the loop decodes, runs it with `Invoke-Expression`,
//! captures output as text and prints a sentinel `<<<UC:<id>:<ok>>>>`. Base64 removes
//! every quoting problem; the sentinel removes every "where does the output end" problem.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use base64::Engine;

const LOOP: &str = r#"[Console]::InputEncoding=[Text.Encoding]::UTF8;[Console]::OutputEncoding=[Text.Encoding]::UTF8;$ErrorActionPreference='Continue';while($true){$l=[Console]::In.ReadLine();if($null -eq $l){break};$sp=$l.IndexOf(' ');$id=$l.Substring(0,$sp);$s=[Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($l.Substring($sp+1)));$ok='1';try{$o=Invoke-Expression $s 2>&1 | Out-String -Width 220;if($o){[Console]::Out.Write($o)};if(-not $?){$ok='0'}}catch{$ok='0';[Console]::Out.WriteLine($_.Exception.Message)};[Console]::Out.WriteLine("<<<UC:$id`:$ok>>>");[Console]::Out.Flush()}"#;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("pwsh not found or failed to start: {0}")]
    Spawn(std::io::Error),
    #[error("command timed out after {0:?}; session killed")]
    Timeout(Duration),
    #[error("session closed")]
    Closed,
    #[error("blocked by guard: {0}")]
    Guarded(&'static str),
}

#[derive(Clone, Debug)]
pub struct Output {
    pub ok: bool,
    pub text: String,
    pub elapsed: Duration,
}

pub struct PowerShell {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

/// Commands that must never run without an explicit human confirmation, whatever Jev
/// says. Matched case-insensitively against whole tokens (a parameter value such as
/// `-Format o` must not trip the guard).
pub const DESTRUCTIVE_TOKENS: [&str; 20] = [
    "remove-item",
    "ri",
    "rm",
    "rmdir",
    "rd",
    "del",
    "erase",
    "format-volume",
    "format",
    "clear-disk",
    "clear-content",
    "clc",
    "stop-computer",
    "restart-computer",
    "remove-itemproperty",
    "diskpart",
    "cipher",
    "remove-partition",
    "initialize-disk",
    "-force",
];

/// Two-token commands (`reg delete`, `wmic … delete`).
pub const DESTRUCTIVE_PAIRS: [(&str, &str); 3] =
    [("reg", "delete"), ("wmic", "delete"), ("git", "clean")];

pub fn is_destructive(script: &str) -> bool {
    let lower = script.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    ';' | '|' | '(' | ')' | '{' | '}' | '&' | '`' | '\'' | '"'
                )
        })
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.iter().any(|t| DESTRUCTIVE_TOKENS.contains(t)) {
        return true;
    }
    tokens.windows(2).any(|w| {
        DESTRUCTIVE_PAIRS
            .iter()
            .any(|(a, b)| w[0] == *a && w[1] == *b)
    })
}

impl PowerShell {
    /// Start the warm session. ~300–600 ms once per run.
    pub fn start() -> Result<Self, ShellError> {
        let mut child = Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-NoLogo", "-Command", LOOP])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(ShellError::Spawn)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("uc-pwsh-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn reader thread");
        Ok(Self {
            child,
            stdin,
            lines: rx,
            next_id: 1,
        })
    }

    /// Run a script and collect its text output. `allow_destructive` must be true for
    /// anything on the [`DESTRUCTIVE`] list — the loop only sets it after a confirmation.
    pub fn run(
        &mut self,
        script: &str,
        timeout: Duration,
        allow_destructive: bool,
    ) -> Result<Output, ShellError> {
        if !allow_destructive && is_destructive(script) {
            return Err(ShellError::Guarded(
                "destructive command requires confirmation",
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let line = format!(
            "{id} {}\n",
            base64::engine::general_purpose::STANDARD.encode(utf16)
        );
        let t0 = Instant::now();
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|_| ShellError::Closed)?;
        self.stdin.flush().map_err(|_| ShellError::Closed)?;
        let sentinel = format!("<<<UC:{id}:");
        let mut text = String::new();
        loop {
            let remaining = timeout.checked_sub(t0.elapsed()).unwrap_or_default();
            match self.lines.recv_timeout(remaining) {
                Ok(l) if l.starts_with(&sentinel) => {
                    let ok = l[sentinel.len()..].starts_with('1');
                    return Ok(Output {
                        ok,
                        text: text.trim_end().to_string(),
                        elapsed: t0.elapsed(),
                    });
                }
                Ok(l) => {
                    text.push_str(&l);
                    text.push('\n');
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ = self.child.kill();
                    return Err(ShellError::Timeout(timeout));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(ShellError::Closed),
            }
        }
    }
}

impl Drop for PowerShell {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::is_destructive;

    #[test]
    fn guard_matches_tokens_not_substrings() {
        assert!(!is_destructive("Get-Date -Format o"));
        assert!(!is_destructive("Get-ChildItem C:\\deleted_items"));
        assert!(is_destructive("Remove-Item -Recurse .\\build"));
        assert!(is_destructive("del *.tmp"));
        assert!(is_destructive("reg delete HKCU\\Foo /f"));
        assert!(is_destructive("Get-Process | Stop-Process -Force"));
    }
}
