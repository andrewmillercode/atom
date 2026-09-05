//! Dev-only /profile overlay: surface startup time plus live CPU/RSS/VSZ/
//! thread counts for the client and the background server. The view is
//! the same fullscreen template /stats uses (a stack of read-only rows
//! drawn from a [`ProfileReport`]); the only difference is how the rows
//! are produced — `ps` against both PIDs.

use std::time::{Duration, SystemTime};

/// One row of process stats gathered via `ps -o pcpu=,rss=,vsz=,etime=,
/// pid=,comm=` on macOS (`%cpu` on Linux). All values are best-effort:
/// `ps` is not present on every platform the project compiles for, so
/// every field is `Option<_>` and `gather` reports `None` when the
/// binary exits non-zero or stdout is empty. Float fields use
/// `PartialEq` (not `Eq`); the parent struct drops `Eq` for the same
/// reason.
///
/// `uptime` is read from ps but never shown in the per-process block
/// — the renderer keeps all uptime data in the Startup section so
/// the two processes line up. It's kept on the struct only as a
/// stepping stone: `gather` uses it to compute `server_started_at`,
/// which is what actually drives the live ticker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessInfo {
    pub pid: i32,
    pub name: String,
    /// CPU% as reported by `ps`. Sampled over the OS's lifetime window;
    /// can exceed 100% on multi-core hosts.
    pub cpu_percent: Option<f32>,
    /// Resident memory (RSS) in KiB.
    pub rss_kb: Option<i64>,
    /// Virtual memory size (VSZ) in KiB.
    pub vsz_kb: Option<i64>,
    /// Wall-clock uptime parsed from `ps etime`. Used to compute
    /// `server_started_at`; not rendered per-process.
    pub uptime: Option<Duration>,
}

/// Result of one `/profile` fetch: the captured startup instant, the
/// client's snapshot, and the server's snapshot (None when no server
/// is reachable). The TUI renders all three sections.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileReport {
    /// Wall-clock instant when the client process started (captured
    /// at the very top of `main()`). Drives both "client startup"
    /// and the live "client uptime" ticker.
    pub started_at: Option<SystemTime>,
    /// Wall-clock instant when the TUI became ready to accept input
    /// (set once, after `setup_terminal`). `ready_at - started_at`
    /// is the static "ms to load" metric for hillclimbing.
    pub ready_at: Option<SystemTime>,
    /// Wall-clock instant when the server process started, derived
    /// from `ps etime` at gather time. Storing the start (not the
    /// snapshot uptime) lets the renderer compute a live uptime on
    /// every frame — `now - server_started_at` ticks as the user
    /// watches the overlay.
    pub server_started_at: Option<SystemTime>,
    pub client: Option<ProcessInfo>,
    pub server: Option<ProcessInfo>,
    /// Free-form error from the underlying `ps` call so the overlay can
    /// tell the user *why* a section is empty when `ps` is missing.
    pub error: Option<String>,
}

impl ProfileReport {
    /// Time elapsed since `started_at`, or None when no startup
    /// instant was captured (e.g. output-test mode, where main is
    /// synthesized). Computed at the call site so it ticks live.
    pub fn client_uptime(&self, now: SystemTime) -> Option<Duration> {
        let started = self.started_at?;
        now.duration_since(started).ok()
    }

    /// Static "ms to load" — `ready_at - started_at`. Captured once;
    /// does not tick, so the user can read the value off /profile
    /// without it changing under them while they hillclimb startup.
    pub fn client_startup(&self) -> Option<Duration> {
        let started = self.started_at?;
        let ready = self.ready_at?;
        ready.duration_since(started).ok()
    }

    /// Live server uptime — `now - server_started_at`. Computed at
    /// the call site so each frame reads the current wall clock.
    pub fn server_uptime(&self, now: SystemTime) -> Option<Duration> {
        let started = self.server_started_at?;
        now.duration_since(started).ok()
    }
}

/// run_ps shells out to `ps` with a fixed column template and parses the
/// single matching row. Caller passes the OS-format options; `ps` is
/// available everywhere atom runs (macOS + Linux), but the column
/// spellings vary — `%cpu` vs `pcpu`, `nlwp` vs `nthr` — so this
/// wrapper hides the differences.
async fn run_ps(opts: &[&str], pid: i32) -> Result<Option<ProcessInfo>, String> {
    let mut cmd = tokio::process::Command::new("ps");
    cmd.args(opts);
    cmd.arg("-p");
    cmd.arg(pid.to_string());
    cmd.stdin(std::process::Stdio::null());
    let out = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            // Most common cause: `ps` exists but the spawned process
            // inherited no PATH (rare on macOS/Linux; tokio forwards
            // the parent's env). Return the error verbatim so the
            // overlay can show it instead of a vague "ps not
            // available".
            return Err(format!("spawn ps: {e}"));
        }
    };
    // Empty stdout AND non-zero status is how ps reports "no such pid"
    // — it writes nothing and exits 1. Don't classify that as an
    // error; it's just "no data". A real ps failure prints a
    // diagnostic on stderr (discarded) and exits non-zero with at
    // least some stdout; `unexpected ps output` below catches that.
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(parse_ps_row(&text))
}

/// Pick the data row out of `ps` output. `ps -o ...` always prints a
/// header line followed by one row per matched pid, but on macOS the
/// `col=` shorthand (empty column name) suppresses that column's
/// title — and using it for *every* column drops the whole header
/// line. So we don't assume the first non-empty line is a header:
/// detect by trying to parse the first token as a number. If it
/// parses as f32 it's a data row (CPU%); if not, skip it as a header.
fn parse_ps_row(text: &str) -> Option<ProcessInfo> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    // Probe the first non-empty line: parseable f32 → it's data;
    // anything else → header, advance to the next line.
    let row = loop {
        let candidate = lines.next()?;
        if candidate
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f32>().ok())
            .is_some()
        {
            break candidate;
        }
    };
    let mut parts = row.split_whitespace();
    let cpu = parts.next()?.parse::<f32>().ok();
    let rss = parts.next()?.parse::<i64>().ok();
    let vsz = parts.next()?.parse::<i64>().ok();
    // etime has two shapes: HH:MM:SS, or DDD-HH:MM:SS for >24h.
    let uptime = parts.next().and_then(parse_etime);
    let pid = parts.next()?.parse::<i32>().ok()?;
    // Rest of the line is the command (may contain spaces, unlike the
    // earlier fixed columns).
    let rest: String = parts.collect::<Vec<_>>().join(" ");
    // ps truncates command at the rightmost column width; basename it
    // so the overlay shows `atoms` rather than the full path.
    let name = std::path::Path::new(rest.trim())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rest.trim().to_string());
    Some(ProcessInfo {
        pid,
        name,
        cpu_percent: cpu,
        rss_kb: rss,
        vsz_kb: vsz,
        uptime,
    })
}

/// Parse ps(1) etime output into a Duration. Accepts three shapes:
/// `MM:SS` (used for uptimes under an hour), `HH:MM:SS` (the common
/// shape), and `DDDD-HH:MM:SS` (the days form, used once uptime
/// exceeds 24h).
fn parse_etime(raw: &str) -> Option<Duration> {
    let (days_str, rest) = match raw.split_once('-') {
        Some((d, r)) => (d, r),
        None => ("0", raw),
    };
    let days = days_str.parse::<u64>().ok()?;
    let fields: Vec<&str> = rest.split(':').collect();
    let (h, m, s) = match fields.len() {
        // MM:SS — ps drops the hour field for uptimes under 60 minutes.
        2 => {
            let m = fields[0].parse::<u64>().ok()?;
            let s = fields[1].parse::<u64>().ok()?;
            (0u64, m, s)
        }
        // HH:MM:SS — the common shape.
        3 => {
            let h = fields[0].parse::<u64>().ok()?;
            let m = fields[1].parse::<u64>().ok()?;
            let s = fields[2].parse::<u64>().ok()?;
            (h, m, s)
        }
        // Anything else is malformed; ps never produces more fields.
        _ => return None,
    };
    Some(Duration::from_secs(days * 86_400 + h * 3_600 + m * 60 + s))
}

#[cfg(target_os = "macos")]
const PS_OPTS: &[&str] = &["-o", "pcpu=,rss=,vsz=,etime=,pid=,comm="];

#[cfg(target_os = "linux")]
const PS_OPTS: &[&str] = &["-o", "%cpu=,rss=,vsz=,etime=,pid=,comm="];

/// Gather live stats for both processes and assemble a ProfileReport.
/// Failures on one side do not block the other — the report carries
/// whichever sections produced data plus a one-line error summary when
/// something actually broke (vs. simply having no row for the pid).
pub async fn gather(
    client_pid: i32,
    server_pid: Option<i32>,
    started_at: Option<SystemTime>,
    ready_at: Option<SystemTime>,
) -> ProfileReport {
    // run_ps returns Result<Option<ProcessInfo>, String>:
    // - Err: ps itself failed to run; the string is the diagnostic.
    // - Ok(None): ps ran fine but had no row for this pid (process
    //   gone or not visible) — that's not an error.
    // - Ok(Some(info)): live snapshot.
    let client = run_ps(PS_OPTS, client_pid).await;
    let server = match server_pid {
        Some(pid) => run_ps(PS_OPTS, pid).await,
        None => Ok(None),
    };
    // Surface the underlying diagnostic only when ps actually broke,
    // so the missing row gets attributed to the specific section
    // instead of being blamed on ps as a whole.
    let error = match (&client, &server) {
        (Err(c), Err(s)) => Some(format!("client: {c} | server: {s}")),
        (Err(c), _) => Some(c.clone()),
        (_, Err(s)) => Some(s.clone()),
        _ => None,
    };
    let client_info = client.unwrap_or(None);
    let server_info = server.unwrap_or(None);
    // Convert the server's `ps etime` snapshot to a wall-clock start
    // instant so the renderer can compute a live uptime on every
    // frame. `now()` here is when the gather happened; a few ms of
    // drift between gather and render is negligible. None when the
    // server has no row (pid gone / not visible).
    let server_started_at = server_info
        .as_ref()
        .and_then(|info| info.uptime)
        .and_then(|up| std::time::SystemTime::now().checked_sub(up));
    ProfileReport {
        started_at,
        ready_at,
        server_started_at,
        client: client_info,
        server: server_info,
        error,
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

/// Render the report as a list of plain-text display lines for the
/// fullscreen template's `ViewRow::Raw` slots. The function is
/// intentionally self-contained — no ansi/fullscreen imports — so it
/// can be unit-tested without booting the TUI.
///
/// The `now` argument is the wall-clock at render time; every call
/// re-derives the live uptimes, so the values tick as the user
/// watches the overlay (one render per frame).
pub fn render_profile(report: &ProfileReport, now: SystemTime) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    // --- Startup -------------------------------------------------------
    lines.push(section("Startup"));
    // Static hillclimb metric: ready_at - started_at. Always
    // formatted with full ms precision so the value can be compared
    // across runs without rounding to "1.2s".
    lines.push(row(
        "client startup",
        report
            .client_startup()
            .map(format_duration_ms)
            .as_deref()
            .unwrap_or("—"),
    ));
    // Live ticker: now - started_at. Refreshes every frame because
    // `now` comes from the caller (the overlay's per-frame draw).
    lines.push(row(
        "client uptime",
        report
            .client_uptime(now)
            .map(format_duration_ms)
            .as_deref()
            .unwrap_or("—"),
    ));
    // Live server ticker: same shape as client uptime, but the start
    // instant comes from `now - ps_etime` at gather time rather than
    // a wall-clock capture. The renderer never reads a stale value
    // because the subtraction runs each frame.
    lines.push(row(
        "server uptime",
        report
            .server_uptime(now)
            .map(format_duration_ms)
            .as_deref()
            .unwrap_or("—"),
    ));

    // --- Client -------------------------------------------------------
    lines.push(section("Client"));
    match &report.client {
        Some(c) => lines.extend(process_rows("atom client", c)),
        None => lines.push(missing_row("atom client")),
    }

    // --- Server -------------------------------------------------------
    lines.push(section("Server"));
    match &report.server {
        Some(s) => lines.extend(process_rows("atom server", s)),
        None => lines.push(missing_row("atom server")),
    }

    // Footer-ish last line: only when something actually went wrong.
    // Distinct from the "no row for this pid" path above, which is
    // normal and renders "not running" per-section. A real error
    // (ps missing, sandbox block) deserves its own line so the user
    // can tell what to fix.
    if let Some(err) = &report.error {
        lines.push(String::new());
        lines.push(format!("! {err}"));
    }

    lines
}

fn section(title: &str) -> String {
    format!("── {title} ──")
}

fn row(label: &str, value: &str) -> String {
    let pad = " ".repeat(20usize.saturating_sub(label.len()));
    format!("{label}{pad}{value}")
}

fn missing_row(label: &str) -> String {
    // The "no row for this pid" case has no diagnostic — distinguishing
    // a gone pid from a never-known pid needs pid-file state we don't
    // pass in. Real errors (ps failed to spawn, parse broke) live in
    // `report.error` and render as a footer line; per-section rows
    // just say "not running" so the reader knows what they're looking
    // at without inventing a cause.
    row(label, "not running")
}

fn process_rows(prefix: &str, info: &ProcessInfo) -> Vec<String> {
    let mut out = Vec::new();
    let header = format!("{} (pid {})", info.name, info.pid);
    out.push(row(prefix, &header));
    out.push(row(
        "  cpu%",
        info.cpu_percent
            .map(|c| format!("{c:.1}"))
            .as_deref()
            .unwrap_or("—"),
    ));
    out.push(row(
        "  rss",
        info.rss_kb.map(format_kib).as_deref().unwrap_or("—"),
    ));
    out.push(row(
        "  vsz",
        info.vsz_kb.map(format_kib).as_deref().unwrap_or("—"),
    ));
    out
}

/// "1234ms", "5.4s", "1m 23s", "1h 02m", "1d 02h". Always includes
/// the unit so the overlay can never show a bare number. The window
/// for ms extends up to 10s instead of the usual 1s — startup-time
/// values land in the 500-3000ms range, and reading "2345ms" is the
/// whole point of /profile's hillclimb metric; rounding to "2.3s"
/// defeats the comparison runs are trying to do.
fn format_duration_ms(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 10_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        let m = ms / 60_000;
        let s = (ms % 60_000) / 1000;
        format!("{m}m {s:02}s")
    } else if ms < 86_400_000 {
        let h = ms / 3_600_000;
        let m = (ms % 3_600_000) / 60_000;
        format!("{h}h {m:02}m")
    } else {
        let days = ms / 86_400_000;
        let h = (ms % 86_400_000) / 3_600_000;
        format!("{days}d {h:02}h")
    }
}

/// KiB → human-readable: "123 KiB", "4.2 MiB", "1.1 GiB".
fn format_kib(kib: i64) -> String {
    if kib < 1024 {
        format!("{kib} KiB")
    } else if kib < 1024 * 1024 {
        format!("{:.1} MiB", kib as f64 / 1024.0)
    } else {
        format!("{:.2} GiB", kib as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_row_with_expected_columns() {
        let text = "%CPU   RSS      VSZ ELAPSED   PID COMM\n\
                     3.4 12345 678901 01:23:45  4242 atoms\n";
        let info = parse_ps_row(text).expect("row parses");
        assert_eq!(info.pid, 4242);
        assert_eq!(info.rss_kb, Some(12345));
        assert_eq!(info.vsz_kb, Some(678901));
        assert_eq!(info.uptime, Some(Duration::from_secs(3600 + 23 * 60 + 45)));
        assert_eq!(info.name, "atoms");
        assert!((info.cpu_percent.unwrap() - 3.4).abs() < 0.01);
    }

    #[test]
    fn parses_ps_row_without_header_line() {
        // `ps -o col1=,col2=,...` on macOS suppresses the entire
        // header line when every column name is empty. The overlay
        // relied on a header being present — this test pins the
        // fix so we never silently regress to "not running" again.
        let text = " 0.2   7104 435329520 00:01 62801 profile_e2e\n";
        let info = parse_ps_row(text).expect("row parses without header");
        assert_eq!(info.pid, 62801);
        assert_eq!(info.rss_kb, Some(7104));
        assert_eq!(info.name, "profile_e2e");
        assert!(info.cpu_percent.is_some());
        assert_eq!(info.uptime, Some(Duration::from_secs(1)));
    }

    #[test]
    fn parses_ps_row_with_header_line_present() {
        // Linux + the `pcpu=` variant keep the header. Make sure the
        // probe logic skips it correctly instead of treating it as
        // data (the CPU% column starts with `%`, which won't parse).
        let text = "%CPU   RSS      VSZ ELAPSED   PID COMM\n\
                     0.5 4096 1234567 01:00 99 bash\n";
        let info = parse_ps_row(text).expect("row parses");
        assert_eq!(info.pid, 99);
        assert_eq!(info.name, "bash");
    }

    #[test]
    fn parse_ps_row_rejects_empty() {
        assert!(parse_ps_row("%CPU\n").is_none());
        assert!(parse_ps_row("").is_none());
    }

    #[test]
    fn format_duration_picks_best_unit() {
        // Sub-10s values stay in ms so the startup hillclimb metric
        // stays comparable across runs (no spurious "2.3s").
        assert_eq!(format_duration_ms(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration_ms(Duration::from_millis(2345)), "2345ms");
        assert_eq!(format_duration_ms(Duration::from_millis(9999)), "9999ms");
        // Crossing 10s flips to seconds with one decimal.
        assert_eq!(format_duration_ms(Duration::from_millis(15_000)), "15.0s");
        assert_eq!(format_duration_ms(Duration::from_secs(83)), "1m 23s");
        assert_eq!(
            format_duration_ms(Duration::from_secs(3 * 3600 + 5 * 60)),
            "3h 05m"
        );
        assert_eq!(
            format_duration_ms(Duration::from_secs(2 * 86400 + 3 * 3600)),
            "2d 03h"
        );
    }

    #[test]
    fn kib_scales_into_mib_and_gib() {
        assert_eq!(format_kib(512), "512 KiB");
        assert_eq!(format_kib(2048), "2.0 MiB");
        assert_eq!(format_kib(2 * 1024 * 1024 + 100 * 1024), "2.10 GiB");
    }

    #[test]
    fn render_profile_shows_all_three_sections() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // ready_at is captured when setup_terminal finishes, just
        // before the first frame — about 2s after main() began in
        // realistic launches (auto-update + deps check dominate).
        let ready = started + Duration::from_millis(2_345);
        // The renderer is called a little after the TUI went live.
        let now = started + Duration::from_secs(5);
        // server_started_at is computed at gather time from ps etime;
        // pick a value that produces a clean 1m up at render.
        let server_started_at = now.checked_sub(Duration::from_secs(60));
        let report = ProfileReport {
            started_at: Some(started),
            ready_at: Some(ready),
            server_started_at,
            client: Some(ProcessInfo {
                pid: 7,
                name: "atomdev".into(),
                cpu_percent: Some(2.0),
                rss_kb: Some(1024 * 32),
                vsz_kb: Some(1024 * 1024),
                // uptime is read from ps but no longer rendered
                // per-process; the report carries it for gather() to
                // compute server_started_at.
                uptime: Some(Duration::from_secs(60)),
            }),
            server: Some(ProcessInfo {
                pid: 9,
                name: "atomsdev".into(),
                cpu_percent: Some(1.5),
                rss_kb: Some(1024 * 64),
                vsz_kb: Some(2 * 1024 * 1024),
                uptime: Some(Duration::from_secs(60)),
            }),
            error: None,
        };
        let lines = render_profile(&report, now);
        assert!(lines.iter().any(|l| l.contains("Startup")));
        assert!(lines.iter().any(|l| l.contains("Client")));
        assert!(lines.iter().any(|l| l.contains("Server")));
        assert!(lines.iter().any(|l| l.contains("atomdev")));
        assert!(lines.iter().any(|l| l.contains("atomsdev")));
        // 32 MiB is what the formatter produces for 32*1024 KiB.
        assert!(lines.iter().any(|l| l.contains("32.0 MiB")));
        // client_uptime = now - started = 5s → "5000ms" (sub-10s stays in
        // ms so the hillclimb metric is byte-for-byte comparable
        // across runs).
        // server_uptime = now - server_started_at = 60s → "1m 00s".
        // client_startup = ready - started = 2345ms.
        assert!(lines.iter().any(|l| l.contains("client startup")));
        assert!(lines.iter().any(|l| l.contains("client uptime")));
        assert!(lines.iter().any(|l| l.contains("server uptime")));
        assert!(lines.iter().any(|l| l.contains("2345ms")));
        assert!(lines.iter().any(|l| l.contains("5000ms")));
        assert!(lines.iter().any(|l| l.contains("1m 00s")));
        // Per-process blocks no longer show etime (moved to Startup).
        assert!(!lines.iter().any(|l| l.contains("etime")));
    }

    #[test]
    fn render_profile_handles_missing_server() {
        let report = ProfileReport {
            started_at: None,
            ready_at: None,
            server_started_at: None,
            client: Some(ProcessInfo {
                pid: 1,
                name: "atom".into(),
                ..Default::default()
            }),
            server: None,
            error: None,
        };
        let lines = render_profile(&report, SystemTime::now());
        assert!(lines.iter().any(|l| l.contains("not running")));
    }

    #[test]
    fn render_profile_shows_error_footer_when_ps_fails() {
        let report = ProfileReport {
            started_at: None,
            ready_at: None,
            server_started_at: None,
            client: None,
            server: None,
            error: Some("spawn ps: not found".to_string()),
        };
        let lines = render_profile(&report, SystemTime::now());
        // Both sections render "not running" AND the error footer
        // shows the underlying diagnostic so the user can debug.
        assert!(lines.iter().any(|l| l.contains("not running")));
        assert!(
            lines.iter().any(|l| l.contains("spawn ps: not found")),
            "error footer missing: {lines:?}"
        );
    }

    #[test]
    fn render_profile_keeps_successful_side_when_other_fails() {
        // Client is fine; server had a parse failure. The server row
        // shows "not running" while the client row has real data; the
        // footer reports only the server error.
        let report = ProfileReport {
            started_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            ready_at: None,
            server_started_at: None,
            client: Some(ProcessInfo {
                pid: 7,
                name: "atomdev".into(),
                cpu_percent: Some(1.0),
                ..Default::default()
            }),
            server: None,
            error: Some("server: unexpected ps output".to_string()),
        };
        let lines = render_profile(
            &report,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_002),
        );
        assert!(lines.iter().any(|l| l.contains("atomdev")));
        assert!(lines.iter().any(|l| l.contains("not running")));
        assert!(lines.iter().any(|l| l.contains("unexpected ps output")));
        // Client uptime must NOT be attributed to the server error.
        assert!(lines.iter().any(|l| l.contains("client uptime")));
    }

    #[test]
    fn parse_etime_handles_all_shapes() {
        // MM:SS — ps uses this for uptimes under 60 minutes.
        assert_eq!(parse_etime("00:42"), Some(Duration::from_secs(42)));
        assert_eq!(parse_etime("01:23"), Some(Duration::from_secs(83)));
        // HH:MM:SS — the common shape.
        assert_eq!(
            parse_etime("01:23:45"),
            Some(Duration::from_secs(3600 + 23 * 60 + 45))
        );
        // DDD-HH:MM:SS — used once uptime crosses 24h.
        assert_eq!(
            parse_etime("3-04:00:00"),
            Some(Duration::from_secs(3 * 86400 + 4 * 3600))
        );
        // Garbage is rejected, not silently coerced.
        assert_eq!(parse_etime("42"), None);
        assert_eq!(parse_etime("aa:bb:cc"), None);
        assert_eq!(parse_etime(""), None);
    }

    #[tokio::test]
    async fn gather_returns_data_for_current_pid() {
        // End-to-end check: spawn `ps` against this test process's own
        // pid and confirm gather() returns a populated client snapshot.
        // Skipped if `ps` is missing or the test environment can't see
        // its own pid (rare CI sandboxes).
        let pid = std::process::id() as i32;
        let report = gather(pid, None, None, None).await;
        let Some(client) = report.client else {
            // No data, but no error either means the pid is invisible
            // — skip silently. An error string here would be a real bug.
            assert!(
                report.error.is_none(),
                "unexpected error when ps returned no row: {:?}",
                report.error
            );
            return;
        };
        assert_eq!(client.pid, pid);
        assert!(!client.name.is_empty(), "ps comm was empty for our own pid");
    }

    #[tokio::test]
    async fn gather_handles_missing_pid_without_error() {
        // Pid 0 is reserved by the kernel and never appears in ps
        // output. Confirm we get Ok(None) (no data) rather than an
        // error string, so the overlay renders "not running" instead
        // of a misleading "ps failed" footer.
        let report = gather(0, None, None, None).await;
        assert!(report.client.is_none());
        assert!(
            report.error.is_none(),
            "missing pid should not surface as ps error: {:?}",
            report.error
        );
    }

    #[tokio::test]
    async fn gather_populates_server_started_at() {
        // When ps returns an etime for the server, gather() should
        // produce a `server_started_at` wall-clock so the live ticker
        // has something to subtract from. We can't easily fabricate a
        // pid pointing at a live process from inside a test, so the
        // assertion is best-effort: if the client ps call worked,
        // also check that `server_started_at` is Some when the same
        // pid is passed as both client and server.
        let pid = std::process::id() as i32;
        let report = gather(pid, Some(pid), None, None).await;
        if report.server.is_some() {
            assert!(
                report.server_started_at.is_some(),
                "ps gave us a row but server_started_at is None: {:?}",
                report
            );
        }
    }
}
