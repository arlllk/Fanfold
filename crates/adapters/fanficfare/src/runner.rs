//! Running a child process, behind a trait so the downloader can be tested without one.

use std::{path::PathBuf, process::Stdio, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{io::AsyncWriteExt, process::Command};

const MAX_CAPTURE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    pub cwd: PathBuf,
    /// The child gets exactly this environment and nothing inherited, so none of
    /// the service's secrets (database password, webhook URL, ...) leak into it.
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RunError {
    #[error("could not start `{program}`: {reason}")]
    Spawn { program: String, reason: String },
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("i/o error talking to the process: {0}")]
    Io(String),
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError>;
}

/// Keep the end of `bytes` (the most recent output) as lossy UTF-8.
fn tail_lossy(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(MAX_CAPTURE_BYTES);
    String::from_utf8_lossy(bytes.get(start..).unwrap_or_default()).into_owned()
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokioProcessRunner;

#[async_trait]
impl ProcessRunner for TokioProcessRunner {
    async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Dropping the future on timeout must not leave the child running.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RunError::Spawn {
                program: spec.program.clone(),
                reason: e.to_string(),
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&spec.stdin)
                .await
                .map_err(|e| RunError::Io(e.to_string()))?;
        }

        let output = tokio::time::timeout(spec.timeout, child.wait_with_output())
            .await
            .map_err(|_| RunError::Timeout(spec.timeout))?
            .map_err(|e| RunError::Io(e.to_string()))?;
        Ok(ProcessOutput {
            stdout: tail_lossy(&output.stdout),
            stderr: tail_lossy(&output.stderr),
            success: output.status.success(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(program: &str, args: &[&str], stdin: &str, timeout: Duration) -> ProcessSpec {
        ProcessSpec {
            program: program.into(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            stdin: stdin.as_bytes().to_vec(),
            cwd: std::env::temp_dir(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            timeout,
        }
    }

    #[tokio::test]
    async fn runs_a_process_feeding_stdin_and_capturing_output() {
        let out = TokioProcessRunner
            .run(&spec(
                "sh",
                &["-c", "cat; echo err >&2"],
                "hello",
                Duration::from_secs(10),
            ))
            .await
            .unwrap();
        assert_eq!(
            (out.stdout.as_str(), out.stderr.trim(), out.success),
            ("hello", "err", true)
        );
    }

    #[tokio::test]
    async fn reports_a_nonzero_exit() {
        let out = TokioProcessRunner
            .run(&spec("sh", &["-c", "exit 3"], "", Duration::from_secs(10)))
            .await
            .unwrap();
        assert!(!out.success);
    }

    #[tokio::test]
    async fn the_environment_is_not_inherited() {
        // SAFETY-free check: the parent has HOME set; the child must not see it.
        let out = TokioProcessRunner
            .run(&spec(
                "sh",
                &["-c", "echo \"[${HOME-unset}]\""],
                "",
                Duration::from_secs(10),
            ))
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "[unset]");
    }

    #[tokio::test]
    async fn kills_a_hung_process_at_the_timeout() {
        let started = std::time::Instant::now();
        let err = TokioProcessRunner
            .run(&spec(
                "sh",
                &["-c", "sleep 30"],
                "",
                Duration::from_millis(200),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let err = TokioProcessRunner
            .run(&spec(
                "/nonexistent/python",
                &[],
                "",
                Duration::from_secs(1),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Spawn { .. }));
    }

    #[test]
    fn captures_keep_the_most_recent_output() {
        let mut big = vec![b'a'; MAX_CAPTURE_BYTES];
        big.extend_from_slice(b"THE END");
        let text = tail_lossy(&big);
        assert_eq!(text.len(), MAX_CAPTURE_BYTES);
        assert!(text.ends_with("THE END"));
    }
}
