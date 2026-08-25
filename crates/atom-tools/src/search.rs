//! grep/glob tools backed by ripgrep, ported from search.go with the
//! execRg indirection preserved as an injectable RgRunner so tests can
//! feed fake output without spawning rg.

use std::time::Duration;

pub const RG_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_GREP_LIMIT: usize = 100;
pub const DEFAULT_GLOB_LIMIT: usize = 200;

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
    async fn run(&self, args: &[String]) -> Result<Vec<u8>, RunError>;
}

/// Runs the real `rg` from PATH in the process cwd with a 30s timeout.
pub struct RealRg;

#[async_trait::async_trait]
impl RgRunner for RealRg {
    fn available(&self) -> bool {
        atom_core::deps::find_in_path("rg").is_some()
    }

    async fn run(&self, args: &[String]) -> Result<Vec<u8>, RunError> {
        let mut cmd = tokio::process::Command::new("rg");
        cmd.args(args);
        if let Ok(dir) = std::env::current_dir() {
            cmd.current_dir(dir);
        }
        match tokio::time::timeout(RG_SEARCH_TIMEOUT, cmd.output()).await {
            Err(_) => Err(RunError::TimedOut),
            Ok(Err(e)) => Err(RunError::failed(None, Vec::new()).with_source(e.to_string())),
            Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
            Ok(Ok(out)) => Err(RunError::failed(
                out.status.code(),
                [out.stdout, out.stderr].concat(),
            )),
        }
    }
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
        "--glob",
        "!.git",
        "--max-filesize",
        "1M",
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
    let mut args: Vec<String> = ["--files", "--color=never", "--hidden", "--glob", "!.git"]
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

async fn run_rg_with(runner: &dyn RgRunner, args: &[String]) -> Result<String, String> {
    if !runner.available() {
        return Err("error: grep/glob require ripgrep (https://github.com/BurntSushi/ripgrep). Install rg, then retry".to_string());
    }
    let out = match runner.run(args).await {
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

pub async fn grep_search_with(arguments: &str, runner: &dyn RgRunner) -> String {
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
    match run_rg_with(runner, &cmd_args).await {
        Err(e) => e,
        Ok(text) => limit_lines(&text, limit),
    }
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

pub async fn glob_search_with(arguments: &str, runner: &dyn RgRunner) -> String {
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
    match run_rg_with(runner, &cmd_args).await {
        Err(e) => e,
        Ok(text) => limit_lines(&text, limit),
    }
}

pub async fn grep_search(arguments: &str) -> String {
    grep_search_with(arguments, &REAL_RG).await
}

pub async fn glob_search(arguments: &str) -> String {
    glob_search_with(arguments, &REAL_RG).await
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
        async fn run(&self, args: &[String]) -> Result<Vec<u8>, RunError> {
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
        let out = grep_search_with(r#"{"pattern":"hello","path":"."}"#, &*fake.clone()).await;
        assert!(out.contains("main.go:1:hello"), "{out}");
        let got = fake.recorded();
        assert!(got.contains(&"--fixed-strings".to_string()), "{got:?}");
    }

    #[tokio::test]
    async fn grep_search_no_match() {
        let fake = FakeRg {
            calls: Mutex::new(Vec::new()),
            result: Err(RunError::failed(Some(1), Vec::new())),
        };
        let out = grep_search_with(r#"{"pattern":"zzz"}"#, &fake).await;
        assert_eq!(out, "No matches found.");
    }

    #[tokio::test]
    async fn grep_error_output_is_surfaced() {
        let fake = FakeRg {
            calls: Mutex::new(Vec::new()),
            result: Err(RunError::failed(Some(2), b"boom".to_vec())),
        };
        let out = grep_search_with(r#"{"pattern":"x"}"#, &fake).await;
        assert_eq!(out, "error running ripgrep: exit status 2\nboom");
    }

    #[tokio::test]
    async fn missing_binary_message() {
        struct None;
        #[async_trait::async_trait]
        impl RgRunner for None {
            fn available(&self) -> bool {
                false
            }
            async fn run(&self, _: &[String]) -> Result<Vec<u8>, RunError> {
                unreachable!()
            }
        }
        let out = grep_search_with(r#"{"pattern":"x"}"#, &None).await;
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
            grep_search_with(r#"{"pattern":"  "}"#, &fake).await,
            "error: pattern is required"
        );
        assert_eq!(
            glob_search_with(r#"{"pattern":""}"#, &fake).await,
            "error: pattern is required"
        );
    }

    fn contains_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }
}
