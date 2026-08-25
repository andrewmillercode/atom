//! vector_search, ported from vector_search.go: pinned Semble invocation
//! through uvx with an injectable SembleRunner (Go's execSemble var).

use crate::search::RunError;
use std::time::Duration;

/// Bundled TOOLS.md embedded like Go's //go:embed.
pub const BUNDLED_TOOLS: &str = include_str!("../../../TOOLS.md");

/// Pin Semble so atom always invokes the same CLI, not whatever is on PATH.
pub const SEMBLE_VERSION: &str = "0.5.5";

const SEMBLE_SEARCH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[async_trait::async_trait]
pub trait SembleRunner: Send + Sync {
    fn available(&self) -> bool;
    /// Runs one command, returning combined output.
    async fn run(&self, args: &[String]) -> Result<Vec<u8>, RunError>;
}

/// Runs the real `uvx` from PATH in the process cwd with a 5m timeout.
pub struct RealSemble;

#[async_trait::async_trait]
impl SembleRunner for RealSemble {
    fn available(&self) -> bool {
        atom_core::deps::find_in_path("uvx").is_some()
    }

    async fn run(&self, args: &[String]) -> Result<Vec<u8>, RunError> {
        let mut cmd = tokio::process::Command::new("uvx");
        cmd.args(args);
        if let Ok(dir) = std::env::current_dir() {
            cmd.current_dir(dir);
        }
        match tokio::time::timeout(SEMBLE_SEARCH_TIMEOUT, cmd.output()).await {
            Err(_) => Err(RunError::TimedOut),
            Ok(Err(e)) => Err(RunError::Failed {
                exit_code: -1,
                output: Vec::new(),
                source: e.to_string(),
            }),
            Ok(Ok(out)) if out.status.success() => Ok([out.stdout, out.stderr].concat()),
            Ok(Ok(out)) => Err(RunError::failed(
                out.status.code(),
                [out.stdout, out.stderr].concat(),
            )),
        }
    }
}

pub static REAL_SEMBLE: RealSemble = RealSemble;

#[derive(serde::Deserialize, Default)]
struct VectorArgs {
    #[serde(default)]
    query: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    top_k: i64,
    #[serde(default)]
    max_snippet_lines: i64,
}

pub async fn vector_search_with(arguments: &str, runner: &dyn SembleRunner) -> String {
    let args: VectorArgs = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    if args.query.trim().is_empty() {
        return "error: query is required".to_string();
    }
    let cmd_args = match semble_search_args(
        &args.query,
        &args.path,
        &args.content,
        args.top_k,
        args.max_snippet_lines,
    ) {
        Ok(a) => a,
        Err(e) => return format!("error: {e}"),
    };
    let out = match runner.run(&cmd_args).await {
        Ok(out) => out,
        Err(RunError::Missing) => {
            return "error: vector_search requires uv (https://docs.astral.sh/uv/). Install uv, then retry."
                .to_string();
        }
        Err(RunError::TimedOut) => {
            return "error: semble search timed out after 5m0s".to_string();
        }
        Err(RunError::Failed { output, source, .. }) => {
            return format!(
                "error running semble search: {}\n{}",
                source,
                String::from_utf8_lossy(&output).trim()
            );
        }
    };
    String::from_utf8_lossy(&out).trim().to_string()
}

pub async fn vector_search(arguments: &str) -> String {
    vector_search_with(arguments, &REAL_SEMBLE).await
}

/// sembleSearchArgs builds `uvx --from semble==<pin> semble search ...`.
/// It only ever invokes search, never find-related.
pub fn semble_search_args(
    query: &str,
    path: &str,
    content: &str,
    top_k: i64,
    max_snippet_lines: i64,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = vec![
        "--from".to_string(),
        format!("semble=={SEMBLE_VERSION}"),
        "semble".to_string(),
        "search".to_string(),
        query.to_string(),
    ];
    if !path.is_empty() {
        args.push(path.to_string());
    }
    match content {
        "" | "code" => {}
        "docs" | "config" | "all" => {
            args.push("--content".to_string());
            args.push(content.to_string());
        }
        _ => return Err("content must be code, docs, config, or all".to_string()),
    }
    if top_k < 0 {
        return Err("top_k must be >= 0".to_string());
    }
    if top_k > 0 {
        args.push("--top-k".to_string());
        args.push(top_k.to_string());
    }
    if max_snippet_lines < 0 {
        return Err("max_snippet_lines must be >= 0".to_string());
    }
    if max_snippet_lines > 0 {
        args.push("--max-snippet-lines".to_string());
        args.push(max_snippet_lines.to_string());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeSemble(Mutex<(Vec<String>, bool)>);

    #[async_trait::async_trait]
    impl SembleRunner for FakeSemble {
        fn available(&self) -> bool {
            true
        }
        async fn run(&self, args: &[String]) -> Result<Vec<u8>, RunError> {
            self.0.lock().unwrap().0 = args.to_vec();
            Ok(b"src/auth.go:12-40\nfunc login() {}\n".to_vec())
        }
    }

    #[test]
    fn args_pin_version_and_only_search() {
        let args = semble_search_args("auth flow", "./repo", "docs", 10, 8).unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains(&format!("semble=={SEMBLE_VERSION}")),
            "{args:?}"
        );
        assert_eq!(&args[2], "semble");
        assert_eq!(&args[3], "search");
        assert!(!args
            .iter()
            .any(|a| a == "find-related" || a == "find_related"));
        assert!(contains_pair(&args, "--content", "docs"));
        assert!(contains_pair(&args, "--top-k", "10"));
        assert!(contains_pair(&args, "--max-snippet-lines", "8"));
    }

    #[test]
    fn rejects_unknown_content_and_negative_limits() {
        assert!(semble_search_args("q", "", "images", 0, 0).is_err());
        assert!(semble_search_args("q", "", "code", -1, 0).is_err());
        assert!(semble_search_args("q", "", "code", 0, -2).is_err());
        // Defaults omit flags.
        let args = semble_search_args("q", "", "", 0, 0).unwrap();
        assert!(!args.contains(&"--content".to_string()));
    }

    #[tokio::test]
    async fn runs_pinned_search_through_uvx() {
        let fake = FakeSemble(Mutex::new((Vec::new(), false)));
        let out =
            vector_search_with(r#"{"query":"login handler","path":".","top_k":3}"#, &fake).await;
        assert!(out.contains("src/auth.go"), "{out}");
        let got = fake.0.lock().unwrap().0.clone();
        assert!(got.contains(&"search".to_string()), "{got:?}");
        assert!(!got.contains(&"find-related".to_string()));
        assert_eq!(&got[0], "--from");
        assert!(got[1].starts_with("semble=="));
    }

    #[test]
    fn bundled_tools_mentions_primers() {
        assert!(BUNDLED_TOOLS.contains("`grep`") && BUNDLED_TOOLS.contains("`glob`"));
        assert!(BUNDLED_TOOLS.to_lowercase().contains("last resort"));
        assert!(BUNDLED_TOOLS.contains("mid-implementation"));
        assert!(
            BUNDLED_TOOLS.contains("web_search")
                && BUNDLED_TOOLS.contains("vector_search")
                && BUNDLED_TOOLS.contains("dispatch")
        );
        let low = BUNDLED_TOOLS.to_lowercase();
        assert!(!low.contains("find_related") && !low.contains("find-related"));
    }

    #[test]
    fn empty_query_rejected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(vector_search_with(r#"{"query":" "}"#, &REAL_SEMBLE));
        assert_eq!(out, "error: query is required");
    }

    fn contains_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }
}
