//! grep/glob tools backed by ripgrep, ported from search.go with the
//! execRg indirection preserved as an injectable RgRunner so tests can
//! feed fake output without spawning rg.

use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub const RG_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_GREP_LIMIT: usize = 100;
pub const DEFAULT_GLOB_LIMIT: usize = 200;
pub const MAX_SEARCH_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_RG_CAPTURE_BYTES: usize = 256 * 1024;

/// Failure modes of one external process run (mirrors how search.go /
/// vector_search.go branch on LookPath, DeadlineExceeded and ExitError).
#[derive(Debug)]
pub enum RunError {
    /// Program not on PATH.
    Missing,
    /// Killed by the tool's timeout.
    TimedOut,
    /// Ran and failed (non-zero exit or spawn error).
    Failed {
        exit_code: i32,
        output: Vec<u8>,
        source: String,
    },
}

impl RunError {
    pub fn failed(exit_code: Option<i32>, output: Vec<u8>) -> Self {
        match exit_code {
            Some(code) => RunError::Failed {
                exit_code: code,
                output,
                source: format!("exit status {code}"),
            },
            None => RunError::Failed {
                exit_code: -1,
                output,
                source: "signal: killed".to_string(),
            },
        }
    }
}

#[async_trait::async_trait]
pub trait RgRunner: Send + Sync {
    fn available(&self) -> bool;
    async fn run(&self, cwd: &Path, args: &[String]) -> Result<Vec<u8>, RunError>;
}

/// Runs the real `rg` from PATH in the session cwd with bounded output.
pub struct RealRg;

#[async_trait::async_trait]
impl RgRunner for RealRg {
    fn available(&self) -> bool {
        atom_core::deps::find_in_path("rg").is_some()
    }

    async fn run(&self, cwd: &Path, args: &[String]) -> Result<Vec<u8>, RunError> {
        let mut cmd = tokio::process::Command::new("rg");
        cmd.args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| RunError::failed(None, Vec::new()).with_source(e.to_string()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_task = tokio::spawn(read_bounded(stdout, MAX_RG_CAPTURE_BYTES));
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_RG_CAPTURE_BYTES));
        let status = match tokio::time::timeout(RG_SEARCH_TIMEOUT, child.wait()).await {
            Err(_) => {
                let _ = child.kill().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(RunError::TimedOut);
            }
            Ok(Err(e)) => return Err(RunError::failed(None, Vec::new()).with_source(e.to_string())),
            Ok(Ok(status)) => status,
        };
        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();
        if status.success() {
            Ok(stdout)
        } else {
            let mut output = stdout;
            output.extend_from_slice(&stderr);
            output.truncate(MAX_RG_CAPTURE_BYTES);
            Err(RunError::failed(status.code(), output))
        }
    }
}

async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin, max: usize) -> Vec<u8> {
    let mut kept = Vec::with_capacity(max.min(8192));
    let mut buf = [0u8; 8192];
    while let Ok(n) = reader.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let remaining = max.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(remaining)]);
    }
    kept
}

impl RunError {
    fn with_source(mut self, source: String) -> Self {
        if let RunError::Failed { source: s, .. } = &mut self {
            *s = source;
        }
        self
    }
}

pub static REAL_RG: RealRg = RealRg;

// ---------------------------------------------------------------------------
// Argument construction.
// ---------------------------------------------------------------------------

/// rgGrepArgs builds a one-shot ripgrep invocation. Literal -F is used
/// unless regex is set, so the SIMD matcher stays on the fast path.
pub fn rg_grep_args(
    pattern: &str,
    path: &str,
    glob: &str,
    regex: bool,
    case_insensitive: bool,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "--color=never",
        "--line-number",
        "--no-heading",
        "--hidden",
        "--no-messages",
        "--glob",
        "!.git",
        "--max-filesize",
        "1M",
        "--max-columns",
        "1000",
        "--max-columns-preview",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if case_insensitive {
        args.push("--ignore-case".to_string());
    } else {
        args.push("--smart-case".to_string());
    }
    if !regex {
        args.push("--fixed-strings".to_string());
    }
    if !glob.is_empty() {
        args.push("--glob".to_string());
        args.push(glob.to_string());
    }
    args.push("--".to_string());
    args.push(pattern.to_string());
    if !path.is_empty() {
        args.push(path.to_string());
    }
    args
}

pub fn rg_glob_args(pattern: &str, path: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--files",
        "--color=never",
        "--hidden",
        "--no-messages",
        "--glob",
        "!.git",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push("--glob".to_string());
    args.push(pattern.to_string());
    if !path.is_empty() {
        args.push(path.to_string());
    }
    args
}

pub(crate) fn limit_lines(text: &str, n: usize) -> String {
    if n == 0 || text.is_empty() || text == "No matches found." {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= n {
        return text.to_string();
    }
    format!(
        "{}\n... truncated, {} more",
        lines[..n].join("\n"),
        lines.len() - n
    )
}

pub(crate) fn limit_bytes(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... truncated at output byte limit");
    text
}

async fn run_rg_with(runner: &dyn RgRunner, cwd: &Path, args: &[String]) -> Result<String, String> {
    if !runner.available() {
        return Err("error: grep/glob require ripgrep (https://github.com/BurntSushi/ripgrep). Install rg, then retry".to_string());
    }
    let out = match runner.run(cwd, args).await {
        Ok(out) => out,
        Err(RunError::Missing) => {
            return Err("error: grep/glob require ripgrep (https://github.com/BurntSushi/ripgrep). Install rg, then retry".to_string())
        }
        Err(RunError::TimedOut) => {
            return Err("error: ripgrep timed out after 30s".to_string());
        }
        Err(RunError::Failed {
            exit_code,
            output,
            source,
        }) => {
            let text = String::from_utf8_lossy(&output).trim().to_string();
            if exit_code == 1 && text.is_empty() {
                return Ok("No matches found.".to_string());
            }
            if !text.is_empty() {
                return Err(format!("error running ripgrep: {source}\n{text}"));
            }
            return Err(format!("error running ripgrep: {source}"));
        }
    };
    let text = String::from_utf8_lossy(&out).trim().to_string();
    if text.is_empty() {
        return Ok("No matches found.".to_string());
    }
    Ok(text)
}

#[derive(serde::Deserialize, Default)]
struct GrepArgs {
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    glob: String,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    head_limit: usize,
}

pub async fn grep_search_with(arguments: &str, cwd: &Path, runner: &dyn RgRunner) -> String {
    let args: GrepArgs = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    if args.pattern.trim().is_empty() {
        return "error: pattern is required".to_string();
    }
    let limit = if args.head_limit == 0 {
        DEFAULT_GREP_LIMIT
    } else {
        args.head_limit
    };
    let cmd_args = rg_grep_args(
        &args.pattern,
        &args.path,
        &args.glob,
        args.regex,
        args.case_insensitive,
    );
    let text = match run_rg_with(runner, cwd, &cmd_args).await {
        Err(e) => e,
        Ok(text) => limit_lines(&text, limit),
    };
    limit_bytes(text, MAX_SEARCH_OUTPUT_BYTES)
}

#[derive(serde::Deserialize, Default)]
struct GlobArgs {
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    head_limit: usize,
}

pub async fn glob_search_with(arguments: &str, cwd: &Path, runner: &dyn RgRunner) -> String {
    let args: GlobArgs = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    if args.pattern.trim().is_empty() {
        return "error: pattern is required".to_string();
    }
    let limit = if args.head_limit == 0 {
        DEFAULT_GLOB_LIMIT
    } else {
        args.head_limit
    };
    let cmd_args = rg_glob_args(&args.pattern, &args.path);
    let text = match run_rg_with(runner, cwd, &cmd_args).await {
        Err(e) => e,
        Ok(text) => limit_lines(&text, limit),
    };
    limit_bytes(text, MAX_SEARCH_OUTPUT_BYTES)
}

pub async fn grep_search(arguments: &str, cwd: &Path) -> String {
    grep_search_with(arguments, cwd, &REAL_RG).await
}

pub async fn glob_search(arguments: &str, cwd: &Path) -> String {
    glob_search_with(arguments, cwd, &REAL_RG).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeRg {
        calls: Mutex<Vec<Vec<String>>>,
        result: Result<Vec<u8>, RunError>,
    }

    impl FakeRg {
        fn ok(output: &[u8]) -> Self {
            FakeRg {
                calls: Mutex::new(Vec::new()),
                result: Ok(output.to_vec()),
            }
        }
        fn recorded(&self) -> Vec<String> {
            self.calls.lock().unwrap().last().cloned().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl RgRunner for FakeRg {
        fn available(&self) -> bool {
            true
        }
        async fn run(&self, _cwd: &Path, args: &[String]) -> Result<Vec<u8>, RunError> {
            self.calls.lock().unwrap().push(args.to_vec());
            // Clone the stored result; Failed carries its own output.
            match &self.result {
                Ok(b) => Ok(b.clone()),
                Err(RunError::TimedOut) => Err(RunError::TimedOut),
                Err(RunError::Missing) => Err(RunError::Missing),
                Err(RunError::Failed {
                    exit_code,
                    output,
                    source,
                }) => Err(RunError::Failed {
                    exit_code: *exit_code,
                    output: output.clone(),
                    source: source.clone(),
                }),
            }
        }
    }

    #[tokio::test]
    async fn grep_args_use_literal_fast_path() {
        let args = rg_grep_args("executeTool", ".", "*.go", false, false);
        assert!(args.contains(&"--fixed-strings".to_string()));
        assert!(contains_pair(&args, "--glob", "*.go"));
        assert!(args.contains(&"--smart-case".to_string()));
    }

    #[tokio::test]
    async fn grep_args_regex_omits_fixed_strings() {
        let args = rg_grep_args("foo|bar", "", "", true, true);
        assert!(!args.contains(&"--fixed-strings".to_string()));
        assert!(args.contains(&"--ignore-case".to_string()));
    }

    #[test]
    fn glob_args_use_files_mode() {
        let args = rg_glob_args("**/*_test.go", "src");
        assert_eq!(args[0], "--files");
        assert!(contains_pair(&args, "--glob", "**/*_test.go"));
        assert!(!args.iter().any(|a| a == "find" || a == "fd"));
    }

    #[tokio::test]
    async fn grep_search_runs_ripgrep_with_literal_flag() {
        let fake = Arc::new(FakeRg::ok(b"main.go:1:hello\nmain.go:2:hello\n"));
        let out = grep_search_with(
            r#"{"pattern":"hello","path":"."}"#,
            Path::new("/workspace"),
            &*fake.clone(),
        )
        .await;
        assert!(out.contains("main.go:1:hello"), "{out}");
        let got = fake.recorded();
        assert!(got.contains(&"--fixed-strings".to_string()), "{got:?}");
    }

    #[tokio::test]
    async fn search_runner_receives_session_cwd() {
        struct CwdRg(Mutex<Option<std::path::PathBuf>>);
        #[async_trait::async_trait]
        impl RgRunner for CwdRg {
            fn available(&self) -> bool {
                true
            }
            async fn run(&self, cwd: &Path, _: &[String]) -> Result<Vec<u8>, RunError> {
                *self.0.lock().unwrap() = Some(cwd.to_path_buf());
                Ok(b"src/lib.rs:1:needle".to_vec())
            }
        }
        let runner = CwdRg(Mutex::new(None));
        let cwd = Path::new("/session/workspace");

        let out = grep_search_with(r#"{"pattern":"needle"}"#, cwd, &runner).await;

        assert!(out.contains("needle"));
        assert_eq!(runner.0.lock().unwrap().as_deref(), Some(cwd));
    }

    #[tokio::test]
    async fn real_search_defaults_to_session_cwd() {
        if !REAL_RG.available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("unique.rs"),
            "const SESSION_NEEDLE: u8 = 1;\n",
        )
        .unwrap();

        let grep = grep_search(r#"{"pattern":"SESSION_NEEDLE"}"#, workspace.path()).await;
        let glob = glob_search(r#"{"pattern":"**/*.rs"}"#, workspace.path()).await;

        assert!(grep.contains("unique.rs:1:"), "{grep}");
        assert_eq!(glob, "unique.rs");
    }

    #[tokio::test]
    async fn grep_search_no_match() {
        let fake = FakeRg {
            calls: Mutex::new(Vec::new()),
            result: Err(RunError::failed(Some(1), Vec::new())),
        };
        let out = grep_search_with(r#"{"pattern":"zzz"}"#, Path::new("/workspace"), &fake).await;
        assert_eq!(out, "No matches found.");
    }

    #[tokio::test]
    async fn grep_error_output_is_surfaced() {
        let fake = FakeRg {
            calls: Mutex::new(Vec::new()),
            result: Err(RunError::failed(Some(2), b"boom".to_vec())),
        };
        let out = grep_search_with(r#"{"pattern":"x"}"#, Path::new("/workspace"), &fake).await;
        assert_eq!(out, "error running ripgrep: exit status 2\nboom");
    }

    #[tokio::test]
    async fn grep_error_output_is_byte_bounded() {
        let fake = FakeRg {
            calls: Mutex::new(Vec::new()),
            result: Err(RunError::failed(
                Some(2),
                vec![b'x'; MAX_SEARCH_OUTPUT_BYTES * 2],
            )),
        };
        let out = grep_search_with(r#"{"pattern":"x"}"#, Path::new("/workspace"), &fake).await;
        assert!(out.len() < MAX_SEARCH_OUTPUT_BYTES + 100, "{}", out.len());
        assert!(out.contains("truncated at output byte limit"));
    }

    #[tokio::test]
    async fn missing_binary_message() {
        struct None;
        #[async_trait::async_trait]
        impl RgRunner for None {
            fn available(&self) -> bool {
                false
            }
            async fn run(&self, _: &Path, _: &[String]) -> Result<Vec<u8>, RunError> {
                unreachable!()
            }
        }
        let out = grep_search_with(r#"{"pattern":"x"}"#, Path::new("/workspace"), &None).await;
        assert!(out.starts_with("error: grep/glob require ripgrep"), "{out}");
    }

    #[test]
    fn limit_lines_truncation_note() {
        let got = limit_lines("a\nb\nc\nd", 2);
        assert!(got.starts_with("a\nb\n") && got.contains("2 more"), "{got}");
        assert_eq!(limit_lines("No matches found.", 5), "No matches found.");
    }

    #[tokio::test]
    async fn empty_pattern_rejected() {
        let fake = FakeRg::ok(b"");
        assert_eq!(
            grep_search_with(r#"{"pattern":"  "}"#, Path::new("/workspace"), &fake).await,
            "error: pattern is required"
        );
        assert_eq!(
            glob_search_with(r#"{"pattern":""}"#, Path::new("/workspace"), &fake).await,
            "error: pattern is required"
        );
    }

    fn contains_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }
}
