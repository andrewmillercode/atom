//! Client-side helpers for talking to the atom session server over its
//! Unix socket (main.go apiGet/apiPost/apiDelete/ensureServer/
//! holdServerAlive). Self-contained raw HTTP/1.1 — no server-side
//! dependencies required.

use anyhow::{anyhow, Context, Result};

use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const LOCAL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The server's Unix socket path.
pub fn socket_path() -> PathBuf {
    atom_core::session::store::socket_path()
}

async fn dial() -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path()).await
}

/// serverRunning reports whether the atom server is listening on its socket.
pub async fn is_running() -> bool {
    dial().await.is_ok()
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 over the Unix socket.
// ---------------------------------------------------------------------------

struct ResponseHead {
    status: u16,
    headers: HashMap<String, String>,
}

async fn write_request(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> std::io::Result<()> {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: atom\r\nConnection: close\r\n");
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    if let Some(b) = body {
        stream.write_all(b).await?;
    }
    stream.flush().await
}

async fn read_head(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<ResponseHead> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find(buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
            buf.drain(..pos + 4);
            return Ok(parse_head(&head));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before response header completed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn parse_head(head: &str) -> ResponseHead {
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    // "HTTP/1.1 200 OK" -> 200
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    ResponseHead { status, headers }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

enum BodyFraming {
    Length(u64),
    Chunked,
    Eof,
}

fn framing_for(headers: &HashMap<String, String>) -> BodyFraming {
    let te_chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if te_chunked {
        return BodyFraming::Chunked;
    }
    match headers
        .get("content-length")
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(n) => BodyFraming::Length(n),
        None => BodyFraming::Eof,
    }
}

async fn read_body(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    framing: BodyFraming,
) -> std::io::Result<Vec<u8>> {
    match framing {
        BodyFraming::Length(n) => {
            while (buf.len() as u64) < n {
                let mut chunk = [0u8; 8192];
                let got = stream.read(&mut chunk).await?;
                if got == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..got]);
            }
            let take = (n as usize).min(buf.len());
            let body = buf[..take].to_vec();
            buf.drain(..take);
            Ok(body)
        }
        BodyFraming::Chunked => {
            let mut out = Vec::new();
            loop {
                let size = read_chunk_size(stream, buf).await?;
                if size == 0 {
                    break;
                }
                while (buf.len() as u64) < size + 2 {
                    let mut chunk = [0u8; 8192];
                    let got = stream.read(&mut chunk).await?;
                    if got == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..got]);
                }
                let take = (size as usize).min(buf.len());
                out.extend_from_slice(&buf[..take]);
                buf.drain(..take);
                // trailing CRLF after each chunk's data
                if buf.starts_with(b"\r\n") {
                    buf.drain(..2);
                }
            }
            Ok(out)
        }
        BodyFraming::Eof => {
            while let Ok(got) = stream.read_buf(buf).await {
                if got == 0 {
                    break;
                }
            }
            Ok(std::mem::take(buf))
        }
    }
}

async fn read_line_crlf(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<String> {
    loop {
        if let Some(pos) = find(buf, b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
            buf.drain(..pos + 2);
            return Ok(line);
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed mid-chunk",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn read_chunk_size(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<u64> {
    let line = read_line_crlf(stream, buf).await?;
    let hex = line.split(';').next().unwrap_or("").trim();
    u64::from_str_radix(hex, 16)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))
}

/// One-shot request returning (status, body bytes).
async fn request(method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>)> {
    match tokio::time::timeout(LOCAL_REQUEST_TIMEOUT, async {
        let mut stream = dial()
            .await
            .with_context(|| format!("dial {}", socket_path().display()))?;
        write_request(&mut stream, method, path, body).await?;
        let mut buf = Vec::new();
        let head = read_head(&mut stream, &mut buf)
            .await
            .context("read response head")?;
        let framing = framing_for(&head.headers);
        let body = read_body(&mut stream, &mut buf, framing)
            .await
            .context("read response body")?;
        Ok::<_, anyhow::Error>((head.status, body))
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow!("local server request timed out: {method} {path}")),
    }
}

fn decode_json(status: u16, body: &[u8]) -> Result<serde_json::Value> {
    if status >= 400 {
        let text = String::from_utf8_lossy(body).trim().to_string();
        return Err(anyhow!("{status}: {text}"));
    }
    if body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(body).map_err(|e| anyhow!("decode response: {e}"))
}

/// apiGet sends a GET request and decodes the JSON response.
pub async fn get(path: &str) -> Result<serde_json::Value> {
    let (status, body) = request("GET", path, None).await?;
    decode_json(status, &body)
}

/// apiPost sends a JSON POST request and decodes the response.
pub async fn post(path: &str, json_body: &serde_json::Value) -> Result<serde_json::Value> {
    let payload = serde_json::to_vec(json_body)?;
    let (status, body) = request("POST", path, Some(&payload)).await?;
    decode_json(status, &body)
}

/// apiPatch sends a JSON PATCH request and decodes the response.
pub async fn patch(path: &str, json_body: &serde_json::Value) -> Result<serde_json::Value> {
    let payload = serde_json::to_vec(json_body)?;
    let (status, body) = request("PATCH", path, Some(&payload)).await?;
    decode_json(status, &body)
}

/// apiDelete sends a DELETE request; 204 with an empty body is success.
pub async fn delete(path: &str) -> Result<serde_json::Value> {
    let (status, body) = request("DELETE", path, None).await?;
    decode_json(status, &body)
}

// ---------------------------------------------------------------------------
// NDJSON streaming (/send, /events).
// ---------------------------------------------------------------------------

/// De-chunker for a chunked/EOF-delimited response body: yields raw
/// payload byte vectors, hiding transfer framing. Content-Length bodies
/// (and EOF-delimited ones) come through as one big pseudo-chunk.
struct BodyDechunker {
    buf: Vec<u8>,
    chunked: bool,
    chunk_left: u64,
    done: bool,
}

impl BodyDechunker {
    fn new(headers: &HashMap<String, String>) -> Self {
        let chunked = headers
            .get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        BodyDechunker {
            buf: Vec::new(),
            chunked,
            chunk_left: 0,
            done: false,
        }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Reads one CRLF-terminated protocol line (chunk sizes/trailers).
    async fn read_proto_line(&mut self, stream: &mut UnixStream) -> std::io::Result<String> {
        loop {
            if let Some(pos) = Self::find(&self.buf, b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
                self.buf.drain(..pos + 2);
                return Ok(line);
            }
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-chunk",
                ));
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    async fn read_exact_more(
        &mut self,
        stream: &mut UnixStream,
        want: usize,
    ) -> std::io::Result<()> {
        while self.buf.len() < want {
            let mut tmp = vec![0u8; (want - self.buf.len()).max(1024)];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-body",
                ));
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
        Ok(())
    }

    /// Next decoded payload chunk, or None at end of body.
    async fn next_payload(&mut self, stream: &mut UnixStream) -> Option<Vec<u8>> {
        if !self.chunked {
            if self.done {
                return None;
            }
            // EOF-delimited: whatever remains until close.
            let mut out = std::mem::take(&mut self.buf);
            loop {
                let mut tmp = [0u8; 16384];
                match stream.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&tmp[..n]),
                }
            }
            self.done = true;
            return Some(out);
        }

        loop {
            if self.chunk_left == 0 {
                // Read the size line (after consuming any pending CRLF
                // terminator implicitly: sizes follow the terminator of
                // the previous chunk, which read_proto_line skips as it
                // scans for CRLF-delimited lines).
                match self.read_proto_line(stream).await {
                    Ok(line) => {
                        let hex = line.split(';').next().unwrap_or("").trim();
                        match u64::from_str_radix(hex, 16) {
                            Ok(0) => {
                                // Terminal chunk: drain trailers until blank.
                                loop {
                                    match self.read_proto_line(stream).await {
                                        Ok(l) if l.is_empty() => break,
                                        Ok(_) => continue,
                                        Err(_) => break,
                                    }
                                }
                                self.done = true;
                                return None;
                            }
                            Ok(size) => self.chunk_left = size,
                            Err(_) => {
                                self.done = true;
                                return None;
                            }
                        }
                    }
                    Err(_) => {
                        self.done = true;
                        return None;
                    }
                }
                continue;
            }
            let want = self.chunk_left.min(16384) as usize;
            if self.read_exact_more(stream, want).await.is_err() {
                self.done = true;
                return None;
            }
            let out: Vec<u8> = self.buf.drain(..want).collect();
            self.chunk_left -= want as u64;
            if self.chunk_left == 0 {
                // Consume the CRLF terminator following this chunk's data.
                while self.buf.len() < 2 {
                    let mut tmp = [0u8; 2];
                    match stream.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                    }
                }
                if self.buf.starts_with(b"\r\n") {
                    self.buf.drain(..2);
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
    }
}

/// Connects and starts an NDJSON stream, forwarding each decoded line to
/// the returned channel until the server closes or decoding fails.
async fn open_stream(
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>> {
    let handshake = tokio::time::timeout(LOCAL_REQUEST_TIMEOUT, async {
        let mut stream = dial()
            .await
            .with_context(|| format!("dial {}", socket_path().display()))?;
        write_request(&mut stream, method, path, body).await?;
        let mut buf = Vec::new();
        let head = read_head(&mut stream, &mut buf)
            .await
            .context("read response head")?;
        Ok::<_, anyhow::Error>((stream, buf, head))
    })
    .await
    .map_err(|_| anyhow!("local server stream timed out: {method} {path}"))?;
    let (mut stream, mut buf, head) = handshake?;
    if head.status >= 400 {
        let framing = framing_for(&head.headers);
        let err_body = read_body(&mut stream, &mut buf, framing)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&err_body).trim().to_string();
        return Err(anyhow!("{}: {text}", head.status));
    }

    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let mut dechunker = BodyDechunker::new(&head.headers);
    dechunker.buf.append(&mut buf);
    tokio::spawn(async move {
        let mut acc: Vec<u8> = Vec::new();
        // When the caller drops the receiver, close the socket promptly
        // so the server sees the disconnect.
        'outer: while let Some(payload) = tokio::select! {
            biased;
            _ = tx.closed() => break 'outer,
            payload = dechunker.next_payload(&mut stream) => payload,
        } {
            acc.extend_from_slice(&payload);
            while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                if tx.is_closed() {
                    break 'outer;
                }
                let line = String::from_utf8_lossy(&acc[..pos]).into_owned();
                acc.drain(..=pos);
                let trimmed = line.trim_end_matches('\r').trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(v) => {
                        if tx.send(v).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => continue,
                }
            }
        }
        // Final unterminated line before EOF, if any.
        let rest = String::from_utf8_lossy(&acc).trim().to_string();
        if !rest.is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&rest) {
                let _ = tx.send(v).await;
            }
        }
    });
    Ok(rx)
}

/// stream_send POSTs /api/sessions/{id}/send and yields the NDJSON event
/// objects as they arrive on the returned channel.
pub async fn stream_send(
    session_id: &str,
    body: &serde_json::Value,
) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>> {
    let payload = serde_json::to_vec(body)?;
    open_stream(
        "POST",
        &format!("/api/sessions/{session_id}/send"),
        Some(&payload),
    )
    .await
}

/// stream_events GETs /api/sessions/{id}/events and yields NDJSON events
/// until the connection closes.
pub async fn stream_events(
    session_id: &str,
) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>> {
    open_stream("GET", &format!("/api/sessions/{session_id}/events"), None).await
}

/// respond_approval answers a sandbox approval prompt.
pub async fn respond_approval(session_id: &str, approval_id: &str, decision: &str) -> Result<()> {
    let path = format!("/approval/{session_id}");
    let body = serde_json::json!({"id": approval_id, "decision": decision});
    let payload = serde_json::to_vec(&body)?;
    let (status, resp) = request("POST", &path, Some(&payload)).await?;
    if status >= 400 {
        let text = String::from_utf8_lossy(&resp).trim().to_string();
        return Err(anyhow!("{status}: {text}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Server lifecycle.
// ---------------------------------------------------------------------------

/// serverSupportsClient reports whether the live server exposes every API
/// this client needs. Older servers 404 on /api/capabilities or omit
/// newer flags (dispatch).
pub async fn server_supports_client() -> bool {
    #[derive(serde::Deserialize)]
    struct Caps {
        #[serde(default)]
        compact: bool,
        #[serde(default)]
        dispatch: bool,
        #[serde(default)]
        mcp: bool,
        #[serde(default)]
        skills: bool,
        #[serde(default)]
        keepalive: bool,
    }
    let caps: Caps = match get("/api/capabilities").await {
        Ok(v) => match serde_json::from_value(v) {
            Ok(c) => c,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    caps.compact && caps.dispatch && caps.mcp && caps.skills && caps.keepalive
}

/// stopBackgroundServer SIGTERMs the pid from server.pid and waits for
/// the socket to disappear (used when recycling a stale-capability server).
pub async fn stop_background_server() {
    if let Ok(b) = std::fs::read(data_dir_pid_path()) {
        if let Ok(pid) = std::str::from_utf8(&b).unwrap_or("").trim().parse::<i32>() {
            if pid > 0 {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
    }
    for _ in 0..50 {
        if !is_running().await {
            let _ = std::fs::remove_file(socket_path());
            let _ = std::fs::remove_file(data_dir_pid_path());
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn data_dir_pid_path() -> PathBuf {
    atom_core::session::store::data_dir().join("server.pid")
}

/// Locate the `atoms` server binary. It lives next to the running `atom`
/// executable (same directory), which works for both dev-symlink and
/// release installs.
fn find_server_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("find own executable")?;
    let dir = exe.parent().context("executable has no parent dir")?;
    let candidate = dir.join("atoms");
    if candidate.is_file() {
        return Ok(candidate);
    }
    // Fallback: look on PATH (handles `cargo install` putting both
    // binaries in ~/.cargo/bin which is already on PATH).
    if let Some(found) = atom_core::deps::find_in_path("atoms") {
        return Ok(found);
    }
    Err(anyhow!(
        "cannot find `atoms` server binary (looked in {} and PATH)",
        dir.display()
    ))
}

/// ensureServer checks whether the atom server is already running. If
/// not, it starts one as a detached background process and polls until
/// it accepts connections (a 5s deadline, not a 5s sleep).
pub async fn ensure_server() -> Result<()> {
    if is_running().await {
        if server_supports_client().await {
            return Ok(());
        }
        // An older server is still bound to the socket and is missing
        // APIs this client needs (compaction, dispatch, …). Recycle it.
        stop_background_server().await;
    }

    // Start the server as a detached background process using the
    // dedicated `atoms` binary. The kernel names the process from the
    // executable filename, so Activity Monitor / ps show "atoms".
    let server_exe = find_server_binary()?;
    let log_dir = atom_core::session::store::data_dir();
    let log_file = log_dir.join("server.log");
    let log_f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .context("open log file")?;
    let mut cmd = std::process::Command::new(&server_exe);
    cmd.env("_ATOM_LAUNCH", "managed");
    use std::os::unix::process::CommandExt;
    cmd.stdout(std::process::Stdio::from(
        log_f.try_clone().context("clone log file handle")?,
    ))
    .stderr(std::process::Stdio::from(log_f));
    unsafe {
        cmd.pre_exec(|| {
            // Detach from the terminal so the server survives the client.
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("start atoms server")?;

    for _ in 0..50 {
        if is_running().await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err(anyhow!(
        "server did not start within 5 seconds (see {})",
        log_file.display()
    ))
}

/// holdServerAlive opens /api/keepalive in the background and reconnects
/// if the server restarts. The idle monitor only counts in-flight HTTP
/// requests; without this a client that has not yet subscribed to
/// /events looks idle. Never returns.
pub async fn hold_server_alive() -> ! {
    loop {
        match dial().await {
            Err(_) => {
                let _ = ensure_server().await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Ok(mut stream) => {
                if write_request(&mut stream, "GET", "/api/keepalive", None)
                    .await
                    .is_err()
                {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
                // Drain until the server closes (it holds the request open).
                let mut discard = Vec::new();
                if stream.read_to_end(&mut discard).await.is_ok() {
                    // Server restarted or went away; loop reconnects.
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}
