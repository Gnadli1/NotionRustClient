//! The AI agent: Claude through the user's own Claude Code CLI (their
//! subscription login), or an API key for Anthropic or any
//! OpenAI-compatible endpoint. Runs on a thread; events come over a channel.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;

use eframe::egui;
use serde::{Deserialize, Serialize};

pub const KEYS_FILE: &str = "ai-keys.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Backend {
    /// `claude -p` with the user's Claude Code login (Pro/Max subscription).
    #[default]
    ClaudeCli,
    /// api.anthropic.com Messages API with an API key.
    Anthropic,
    /// `{base_url}/chat/completions` (OpenAI, OpenRouter, Mistral, Groq,
    /// Gemini, Ollama, ...).
    OpenAiCompatible,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub backend: Backend,
    /// The Claude Code binary (ClaudeCli).
    pub claude_path: String,
    /// "" = the backend's default model.
    pub model: String,
    /// OpenAiCompatible only, e.g. "https://api.openai.com/v1".
    pub base_url: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            backend: Backend::ClaudeCli,
            claude_path: "claude".into(),
            model: String::new(),
            base_url: "https://api.openai.com/v1".into(),
        }
    }
}

/// API keys, in `ai-keys.json` (mode 600), never in settings.json or logs.
/// Keyed by "anthropic" or the OpenAI-compatible base URL.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Secrets {
    pub keys: BTreeMap<String, String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("keys", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Secrets {
    /// The key slot for `cfg`'s backend; None for the CLI.
    pub fn slot(cfg: &AiConfig) -> Option<String> {
        match cfg.backend {
            Backend::ClaudeCli => None,
            Backend::Anthropic => Some("anthropic".into()),
            Backend::OpenAiCompatible => Some(cfg.base_url.trim().trim_end_matches('/').to_string()),
        }
    }
}

pub fn load_secrets(dir: &Path) -> Secrets {
    std::fs::read_to_string(dir.join(KEYS_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_secrets(dir: &Path, s: &Secrets) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{KEYS_FILE}.tmp"));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    #[cfg(unix)]
    std::fs::set_permissions(
        &tmp,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    let r = f
        .write_all(&serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)
        .and_then(|()| f.sync_all())
        .and_then(|()| std::fs::rename(&tmp, dir.join(KEYS_FILE)));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

#[derive(Debug, Clone, PartialEq)]
pub enum AiEvent {
    /// More answer text (append).
    Text(String),
    Done,
    Error(String),
}

/// A running request. Dropping it cancels it.
pub struct Job {
    pub rx: mpsc::Receiver<AiEvent>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel()
    }
}

const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const CLI_HINT: &str = "Claude Code is not signed in: run `claude` once in a terminal and sign in";

/// Starts one request: `system` is the instructions, `prompt` the user's
/// question with the page. `key` is the API key (None for the CLI).
/// `ctx` is repainted on every event.
pub fn start(
    cfg: &AiConfig,
    key: Option<String>,
    system: &str,
    prompt: &str,
    ctx: egui::Context,
) -> Job {
    let (tx, rx) = mpsc::channel();
    let cancel: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
    let (cfg, system, prompt, flag) = (cfg.clone(), system.to_string(), prompt.to_string(), cancel.clone());
    std::thread::spawn(move || {
        let send = |e: AiEvent| {
            let _ = tx.send(e);
            ctx.request_repaint();
        };
        run(&cfg, key, &system, &prompt, &flag, &send, ANTHROPIC_BASE);
    });
    Job { rx, cancel }
}

fn cancelled(c: &std::sync::atomic::AtomicBool) -> bool {
    c.load(std::sync::atomic::Ordering::Relaxed)
}

fn run(
    cfg: &AiConfig,
    key: Option<String>,
    system: &str,
    prompt: &str,
    cancel: &std::sync::atomic::AtomicBool,
    send: &dyn Fn(AiEvent),
    anthropic_base: &str,
) {
    let secret = key.clone().filter(|k| !k.is_empty());
    let r = match cfg.backend {
        Backend::ClaudeCli => run_cli(cfg, system, prompt, cancel, send),
        Backend::Anthropic => block_on(run_anthropic(cfg, key, system, prompt, cancel, send, anthropic_base)),
        Backend::OpenAiCompatible => block_on(run_openai(cfg, key, system, prompt, cancel, send)),
    };
    match r {
        Err(e) => send(AiEvent::Error(match secret {
            Some(k) => e.replace(&k, "***"),
            None => e,
        })),
        Ok(()) if !cancelled(cancel) => send(AiEvent::Done),
        Ok(()) => {}
    }
}

fn block_on<F: std::future::Future<Output = Result<(), String>>>(f: F) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(f)
}

// ---- Claude Code CLI ----

enum Cli {
    Text(String),
    Result { is_error: bool, text: String },
}

fn cli_line(line: &str) -> Option<Cli> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match v["type"].as_str()? {
        "stream_event" => {
            let d = &v["event"]["delta"];
            let ok = v["event"]["type"] == "content_block_delta" && d["type"] == "text_delta";
            ok.then(|| d["text"].as_str().map(|t| Cli::Text(t.into()))).flatten()
        }
        "result" => Some(Cli::Result {
            is_error: v["is_error"].as_bool().unwrap_or(false),
            text: v["result"].as_str().unwrap_or_default().into(),
        }),
        _ => None,
    }
}

fn cli_err(msg: &str, stderr: &str) -> String {
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let all = format!("{msg} {last}").to_lowercase();
    if all.contains("not logged in") || all.contains("/login") || all.contains("log in") {
        return CLI_HINT.into();
    }
    match (msg.is_empty(), last.is_empty()) {
        (false, false) => format!("{msg} ({last})"),
        (false, true) => msg.into(),
        (true, false) => format!("Claude Code failed: {last}"),
        _ => "Claude Code failed".into(),
    }
}

fn run_cli(
    cfg: &AiConfig,
    system: &str,
    prompt: &str,
    cancel: &std::sync::atomic::AtomicBool,
    send: &dyn Fn(AiEvent),
) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&cfg.claude_path);
    cmd.args([
        "-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages",
        "--tools", "", "--strict-mcp-config", "--no-session-persistence",
        "--append-system-prompt", system,
    ]);
    if !cfg.model.is_empty() {
        cmd.arg(format!("--model={}", cfg.model));
    }
    let mut child = cmd
        .current_dir(private_cwd()?)
        .env_remove("NOTION_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "Claude Code not found at '{}': install it or set its path in Settings \u{2192} AI Agent",
                cfg.claude_path
            ),
            _ => format!("could not start Claude Code: {e}"),
        })?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let prompt = prompt.to_string();
    // Own threads so a big prompt or chatty stderr can't deadlock the pipes.
    std::thread::spawn(move || {
        let _ = stdin.write_all(prompt.as_bytes());
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_string(&mut s);
        s
    });
    // The pipe is read on its own thread so cancel is noticed within 100 ms
    // even when the CLI is silent.
    let (ltx, lrx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if ltx.send(line).is_err() {
                break;
            }
        }
    });
    let (mut seen, mut fail, mut fallback) = (false, None, None);
    loop {
        let line = match lrx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) if !cancelled(cancel) => continue,
            Err(_) => break,
        };
        if cancelled(cancel) {
            break;
        }
        match cli_line(&line) {
            Some(Cli::Text(t)) => {
                seen = true;
                send(AiEvent::Text(t));
            }
            Some(Cli::Result { is_error: true, text }) => fail = Some(text),
            Some(Cli::Result { text, .. }) => fallback = Some(text),
            None => {}
        }
    }
    if cancelled(cancel) || fail.is_some() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if cancelled(cancel) {
        return Ok(());
    }
    let stderr = err_t.join().unwrap_or_default();
    if let Some(m) = fail {
        return Err(cli_err(&m, &stderr));
    }
    if !status.success() {
        return Err(cli_err("", &stderr));
    }
    if let (false, Some(t)) = (seen, fallback) {
        if !t.is_empty() {
            send(AiEvent::Text(t));
        }
    }
    Ok(())
}

// ---- HTTP streaming (SSE) ----

/// Splits a byte stream into lines across chunk boundaries.
#[derive(Default)]
struct LineBuf(Vec<u8>);

impl LineBuf {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.0.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.0.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.0.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string());
        }
        out
    }
}

fn sse_data(line: &str) -> Option<&str> {
    line.strip_prefix("data:").map(str::trim)
}

fn sse_anthropic(line: &str) -> Option<AiEvent> {
    let v: serde_json::Value = serde_json::from_str(sse_data(line)?).ok()?;
    match v["type"].as_str()? {
        "content_block_delta" if v["delta"]["type"] == "text_delta" => {
            Some(AiEvent::Text(v["delta"]["text"].as_str()?.into()))
        }
        "message_delta" if v["delta"]["stop_reason"] == "refusal" => {
            Some(AiEvent::Error("Claude declined this request".into()))
        }
        "message_stop" => Some(AiEvent::Done),
        "error" => Some(AiEvent::Error(
            v["error"]["message"].as_str().unwrap_or("API error").into(),
        )),
        _ => None,
    }
}

fn sse_openai(line: &str) -> Option<AiEvent> {
    let d = sse_data(line)?;
    if d == "[DONE]" {
        return Some(AiEvent::Done);
    }
    let v: serde_json::Value = serde_json::from_str(d).ok()?;
    if let Some(m) = v["error"]["message"].as_str() {
        return Some(AiEvent::Error(m.into()));
    }
    match v["choices"][0]["delta"]["content"].as_str() {
        Some(t) if !t.is_empty() => Some(AiEvent::Text(t.into())),
        _ => None,
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // never forward a key to a redirect target
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.without_url().to_string())
}

/// Awaits `fut` in 250 ms slices so cancel is noticed; None = cancelled,
/// Err after 120 s without a result.
async fn patient<F: std::future::Future>(
    fut: F,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Option<F::Output>, String> {
    let start = std::time::Instant::now();
    tokio::pin!(fut);
    loop {
        if cancelled(cancel) {
            return Ok(None);
        }
        match tokio::time::timeout(std::time::Duration::from_millis(250), &mut fut).await {
            Ok(v) => return Ok(Some(v)),
            Err(_) if start.elapsed() > std::time::Duration::from_secs(120) => {
                return Err("no reply for 120 s".into())
            }
            Err(_) => {}
        }
    }
}

async fn stream(
    req: reqwest::RequestBuilder,
    parse: fn(&str) -> Option<AiEvent>,
    cancel: &std::sync::atomic::AtomicBool,
    send: &dyn Fn(AiEvent),
) -> Result<(), String> {
    let Some(resp) = patient(req.send(), cancel).await? else { return Ok(()) };
    let mut resp = resp.map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        let st = resp.status();
        let body = patient(resp.text(), cancel).await?.and_then(Result::ok).unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(String::from))
            .unwrap_or_default();
        return Err(format!("HTTP {st}: {msg}"));
    }
    let mut buf = LineBuf::default();
    loop {
        let Some(chunk) = patient(resp.chunk(), cancel).await? else { return Ok(()) };
        let chunk = chunk.map_err(|e| e.without_url().to_string())?;
        let end = chunk.is_none();
        let lines = buf.push(chunk.as_deref().unwrap_or(b"\n"));
        for l in lines {
            match parse(&l) {
                Some(AiEvent::Text(t)) => send(AiEvent::Text(t)),
                Some(AiEvent::Error(e)) => return Err(e),
                Some(AiEvent::Done) => return Ok(()),
                None => {}
            }
        }
        if end {
            return Err("the answer was cut off".into());
        }
    }
}

async fn run_anthropic(
    cfg: &AiConfig,
    key: Option<String>,
    system: &str,
    prompt: &str,
    cancel: &std::sync::atomic::AtomicBool,
    send: &dyn Fn(AiEvent),
    base: &str,
) -> Result<(), String> {
    let key = key.filter(|k| !k.is_empty()).ok_or("no Anthropic API key: add one in Settings \u{2192} AI Agent")?;
    let model = if cfg.model.is_empty() { "claude-opus-5-5" } else { &cfg.model };
    let body = serde_json::json!({
        "model": model, "max_tokens": 64000, "stream": true, "fallbacks": "default",
        "system": system,
        "messages": [{"role": "user", "content": prompt}],
    });
    let req = client()?
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "server-side-fallback-2026-07-01")
        .json(&body);
    stream(req, sse_anthropic, cancel, send).await
}

/// https, or plain http only to this machine (never send a key in clear to another host).
fn check_base(url: &str) -> Result<String, String> {
    use reqwest::Url;
    use std::net::IpAddr;
    let u = url.trim().trim_end_matches('/');
    let bad = || Err("the base URL must start with https:// (or be http://localhost)".to_string());
    let Ok(p) = Url::parse(u) else { return bad() };
    if !p.username().is_empty() || p.password().is_some() || !u[p.scheme().len()..].starts_with("://") {
        return bad();
    }
    let Some(h) = p.host_str() else { return bad() };
    let loopback = h == "localhost"
        || h.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    match p.scheme() {
        "https" => Ok(u.to_string()),
        "http" if loopback => Ok(u.to_string()),
        _ => bad(),
    }
}

async fn run_openai(
    cfg: &AiConfig,
    key: Option<String>,
    system: &str,
    prompt: &str,
    cancel: &std::sync::atomic::AtomicBool,
    send: &dyn Fn(AiEvent),
) -> Result<(), String> {
    let base = check_base(&cfg.base_url)?;
    if cfg.model.is_empty() {
        return Err("set a model in Settings \u{2192} AI Agent".into());
    }
    let body = serde_json::json!({
        "model": cfg.model, "stream": true,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
    });
    let mut req = client()?.post(format!("{base}/chat/completions")).json(&body);
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    stream(req, sse_openai, cancel, send).await
}

/// One-line check of the backend for the settings tab ("Claude Code 2.1.3",
/// or the error). Blocking; run it off the UI thread.
pub fn test(cfg: &AiConfig, key: Option<&str>) -> Result<String, String> {
    let key = key.filter(|k| !k.is_empty());
    match cfg.backend {
        Backend::ClaudeCli => {
            let out = std::process::Command::new(&cfg.claude_path)
                .arg("--version")
                .output()
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => format!(
                        "Claude Code not found at '{}': install it or set its path in Settings \u{2192} AI Agent",
                        cfg.claude_path
                    ),
                    _ => format!("could not start Claude Code: {e}"),
                })?;
            let text = String::from_utf8_lossy(&out.stdout);
            match (out.status.success(), text.lines().next()) {
                (true, Some(l)) => Ok(l.trim().to_string()),
                _ => Err(cli_err("", &String::from_utf8_lossy(&out.stderr))),
            }
        }
        Backend::Anthropic => key.map(|_| "key saved".to_string()).ok_or_else(|| "no API key".into()),
        Backend::OpenAiCompatible => {
            let base = check_base(&cfg.base_url)?;
            let key = key.map(String::from);
            let fut = async move {
                let mut req = client()?.get(format!("{base}/models"));
                if let Some(k) = key {
                    req = req.bearer_auth(k);
                }
                let resp = req.send().await.map_err(|e| e.without_url().to_string())?;
                let st = resp.status();
                let v: serde_json::Value = resp.json().await.unwrap_or_default();
                if !st.is_success() {
                    return Err(format!("HTTP {st}: {}", v["error"]["message"].as_str().unwrap_or("")));
                }
                Ok(format!("{} models", v["data"].as_array().map_or(0, |a| a.len())))
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(fut)
        }
    }
}

/// An empty dir only this user can write (0700) for the CLI to run in: no
/// project CLAUDE.md or hooks, and not the shared /tmp where others could
/// plant them.
fn private_cwd() -> Result<std::path::PathBuf, String> {
    // under the user's data dir (~/.local/share/...), never the shared temp dir
    let dir = notion_core::paths::data_dir()
        .ok_or("cannot find the app's data folder")?
        .join("ai-cwd");
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    fn collect(cfg: &AiConfig, key: Option<&str>, base: &str) -> Vec<AiEvent> {
        let ev = Mutex::new(Vec::new());
        run(cfg, key.map(String::from), "SYS", "hi", &AtomicBool::new(false), &|e| ev.lock().unwrap().push(e), base);
        ev.into_inner().unwrap()
    }

    /// Serves one canned response (split mid-line) and returns (port, request text).
    fn serve(status: &str, body: &'static str) -> (u16, mpsc::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = Vec::new();
            let mut b = [0u8; 4096];
            loop {
                let n = s.read(&mut b).unwrap();
                req.extend_from_slice(&b[..n]);
                let t = String::from_utf8_lossy(&req).to_string();
                if let Some(h) = t.find("\r\n\r\n") {
                    let cl = t.to_lowercase().split("content-length:").nth(1)
                        .and_then(|r| r.lines().next()?.trim().parse::<usize>().ok()).unwrap_or(0);
                    if req.len() >= h + 4 + cl {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&req).to_string());
            let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
            let (a, z) = body.split_at(body.len() / 2);
            let _ = s.write_all(a.as_bytes());
            let _ = s.flush();
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = s.write_all(z.as_bytes());
        });
        (port, rx)
    }

    fn texts(ev: &[AiEvent]) -> String {
        ev.iter().filter_map(|e| if let AiEvent::Text(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn parsers() {
        let l = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#;
        assert!(matches!(cli_line(l), Some(Cli::Text(t)) if t == "Hi"));
        assert!(matches!(cli_line(r#"{"type":"result","is_error":true,"result":"x"}"#), Some(Cli::Result { is_error: true, .. })));
        assert!(cli_line("garbage").is_none());
        let a = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"A"}}"#;
        assert_eq!(sse_anthropic(a), Some(AiEvent::Text("A".into())));
        assert_eq!(sse_anthropic("event: ping"), None);
        assert_eq!(
            sse_anthropic(r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal"}}"#),
            Some(AiEvent::Error("Claude declined this request".into()))
        );
        assert_eq!(
            sse_anthropic(r#"data: {"type":"error","error":{"message":"boom"}}"#),
            Some(AiEvent::Error("boom".into()))
        );
        assert_eq!(sse_openai(r#"data: {"choices":[{"delta":{"content":"O"}}]}"#), Some(AiEvent::Text("O".into())));
        assert_eq!(sse_openai("data: [DONE]"), Some(AiEvent::Done));
        assert_eq!(sse_openai(r#"data: {"choices":[{"delta":{}}]}"#), None);
        let mut b = LineBuf::default();
        assert!(b.push(b"data: {\"a").is_empty());
        assert_eq!(b.push(b"\":1}\r\nx\n"), vec!["data: {\"a\":1}", "x"]);
    }

    #[test]
    fn base_url_check() {
        for ok in ["https://api.openai.com/v1/", "http://localhost:11434/v1", "http://127.0.0.1/v1", "http://[::1]:1/v1"] {
            assert!(check_base(ok).is_ok(), "{ok}");
        }
        for bad in ["http://example.com/v1", "http://localhost.evil.com/v1", "http://localhost@evil.com", "http://localhost:80@evil.com/v1", "http://127.0.0.1:1@evil.com", "https://user:pw@api.openai.com", "ftp://x", ""] {
            assert!(check_base(bad).is_err(), "{bad}");
        }
        let cfg = AiConfig { backend: Backend::OpenAiCompatible, base_url: "http://example.com".into(), model: "m".into(), ..Default::default() };
        let ev = collect(&cfg, Some("sekret"), ANTHROPIC_BASE);
        assert!(matches!(&ev[..], [AiEvent::Error(e)] if !e.contains("sekret")));
        let cfg = AiConfig { model: String::new(), base_url: "https://x".into(), ..cfg };
        assert!(matches!(&collect(&cfg, None, "")[..], [AiEvent::Error(e)] if e.contains("set a model")));
    }

    #[test]
    fn secrets_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("sub");
        assert_eq!(load_secrets(&dir), Secrets::default());
        let mut s = Secrets::default();
        s.keys.insert("anthropic".into(), "sk-SECRET".into());
        save_secrets(&dir, &s).unwrap();
        assert_eq!(load_secrets(&dir), s);
        assert!(!format!("{s:?}").contains("sk-SECRET"));
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(dir.join(KEYS_FILE)).unwrap().permissions().mode_bits(), 0o600);
        std::fs::write(dir.join(KEYS_FILE), "{bad").unwrap();
        assert_eq!(load_secrets(&dir), Secrets::default());
    }

    #[cfg(unix)]
    trait Bits {
        fn mode_bits(&self) -> u32;
    }
    #[cfg(unix)]
    impl Bits for std::fs::Permissions {
        fn mode_bits(&self) -> u32 {
            std::os::unix::fs::PermissionsExt::mode(self) & 0o777
        }
    }

    #[cfg(unix)]
    #[test]
    fn cli_backend() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, body: &str| {
            let p = d.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p.to_string_lossy().to_string()
        };
        let good = mk("good", r#"cat >/dev/null
echo '{"type":"system"}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"lo"}}}'
echo '{"type":"result","is_error":false,"result":"Hello"}'"#);
        let nodelta = mk("nodelta", r#"cat >/dev/null
echo '{"type":"result","is_error":false,"result":"Whole"}'"#);
        let bad = mk("bad", "cat >/dev/null\necho 'Not logged in' >&2\nexit 1");
        let cfg = |p: &str| AiConfig { claude_path: p.into(), ..Default::default() };
        let ev = collect(&cfg(&good), None, "");
        assert_eq!(ev, vec![AiEvent::Text("Hel".into()), AiEvent::Text("lo".into()), AiEvent::Done]);
        assert_eq!(collect(&cfg(&nodelta), None, ""), vec![AiEvent::Text("Whole".into()), AiEvent::Done]);
        assert!(matches!(&collect(&cfg(&bad), None, "")[..], [AiEvent::Error(e)] if e.contains("run `claude` once")));
        let missing = collect(&cfg("/nonexistent/claude"), None, "");
        assert!(matches!(&missing[..], [AiEvent::Error(e)] if e.contains("Claude Code not found at '/nonexistent/claude'")));
        assert_eq!(test(&cfg(&mk("ver", "echo 'Claude Code 9.9'")), None), Ok("Claude Code 9.9".into()));
    }

    #[test]
    fn openai_stream() {
        let (port, req) = serve("200 OK", "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n\n");
        let cfg = AiConfig { backend: Backend::OpenAiCompatible, base_url: format!("http://127.0.0.1:{port}/v1/"), model: "m".into(), ..Default::default() };
        let ev = collect(&cfg, Some("k123"), "");
        assert_eq!(texts(&ev), "Hello");
        assert_eq!(ev.last(), Some(&AiEvent::Done));
        let r = req.recv().unwrap().to_lowercase();
        assert!(r.starts_with("post /v1/chat/completions") && r.contains("authorization: bearer k123"));
    }

    #[test]
    fn anthropic_stream_and_errors() {
        let (port, req) = serve("200 OK", "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Yo\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        let cfg = AiConfig { backend: Backend::Anthropic, ..Default::default() };
        let ev = collect(&cfg, Some("sk-1"), &format!("http://127.0.0.1:{port}"));
        assert_eq!(ev, vec![AiEvent::Text("Yo".into()), AiEvent::Done]);
        let r = req.recv().unwrap().to_lowercase();
        assert!(r.contains("x-api-key: sk-1") && r.contains("anthropic-version: 2023-06-01") && r.contains("claude-opus-5-5"));
        let (port, _r) = serve("401 Unauthorized", "{\"error\":{\"message\":\"bad key\"}}");
        let ev = collect(&cfg, Some("sk-2"), &format!("http://127.0.0.1:{port}"));
        assert!(matches!(&ev[..], [AiEvent::Error(e)] if e.starts_with("HTTP 401") && e.contains("bad key") && !e.contains("sk-2")));
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_a_chatty_cli_and_reaps_it() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (script, pidf) = (d.path().join("chatty"), d.path().join("pid"));
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}}"#;
        std::fs::write(&script, format!("#!/bin/sh\necho $$ > '{}'\nwhile :; do echo '{line}'; sleep 0.02; done\n", pidf.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = AiConfig { claude_path: script.to_string_lossy().into(), ..Default::default() };
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let ev = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (c2, e2) = (cancel.clone(), ev.clone());
        let t = std::thread::spawn(move || run(&cfg, None, "S", "p", &c2, &|e| e2.lock().unwrap().push(e), ""));
        while ev.lock().unwrap().is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        t.join().unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(3));
        let pid = std::fs::read_to_string(&pidf).unwrap().trim().to_string();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "child still there (zombie or alive)");
        let ev = ev.lock().unwrap();
        assert!(ev.iter().all(|e| matches!(e, AiEvent::Text(_))), "no Done/Error after cancel: {ev:?}");
    }

    #[cfg(unix)]
    #[test]
    fn cancel_kills_a_silent_cli() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("silent");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = AiConfig { claude_path: script.to_string_lossy().into(), ..Default::default() };
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let t = std::thread::spawn(move || run(&cfg, None, "S", "p", &c2, &|_| {}, ""));
        std::thread::sleep(std::time::Duration::from_millis(300));
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        t.join().unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let (port, _r) = serve("200 OK", "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n");
        let cfg = AiConfig { backend: Backend::OpenAiCompatible, base_url: format!("http://127.0.0.1:{port}/v1"), model: "m".into(), ..Default::default() };
        let ev = collect(&cfg, None, "");
        assert_eq!(ev, vec![AiEvent::Text("Hi".into()), AiEvent::Error("the answer was cut off".into())]);
    }

    #[test]
    fn job_cancel_sets_flag() {
        let (_tx, rx) = mpsc::channel();
        let j = Job { rx, cancel: Default::default() };
        assert!(!cancelled(&j.cancel));
        j.cancel();
        assert!(cancelled(&j.cancel));
    }

    #[test]
    fn openai_without_key_sends_no_authorization() {
        let (port, req) = serve("200 OK", "data: [DONE]\n\n");
        let cfg = AiConfig { backend: Backend::OpenAiCompatible, base_url: format!("http://127.0.0.1:{port}/v1"), model: "m".into(), ..Default::default() };
        assert_eq!(collect(&cfg, Some(""), ""), vec![AiEvent::Done]);
        assert!(!req.recv().unwrap().to_lowercase().contains("authorization"));
    }

    #[test]
    fn key_never_in_error_strings() {
        // server echoes the key in the error message
        let (port, _r) = serve("401 Unauthorized", "{\"error\":{\"message\":\"bad key sk-LEAK\"}}");
        let cfg = AiConfig { backend: Backend::OpenAiCompatible, base_url: format!("http://127.0.0.1:{port}/v1"), model: "m".into(), ..Default::default() };
        let ev = collect(&cfg, Some("sk-LEAK"), "");
        assert!(matches!(&ev[..], [AiEvent::Error(e)] if e.starts_with("HTTP 401") && !e.contains("sk-LEAK") && e.contains("***")), "{ev:?}");
        // connection refused: reqwest error must not carry key or URL
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let cfg = AiConfig { backend: Backend::Anthropic, ..Default::default() };
        let ev = collect(&cfg, Some("sk-LEAK"), &format!("http://127.0.0.1:{port}"));
        assert!(matches!(&ev[..], [AiEvent::Error(e)] if !e.contains("sk-LEAK") && !e.contains("127.0.0.1")), "{ev:?}");
        // no key at all
        let ev = collect(&cfg, None, "");
        assert!(matches!(&ev[..], [AiEvent::Error(e)] if e.contains("no Anthropic API key")));
    }

    #[test]
    fn check_base_edge_cases() {
        for bad in ["https://", "http://", "http://localhost.evil.com", "http://127.0.0.1.evil.com", "http://localhostx", "https:/x"] {
            assert!(check_base(bad).is_err(), "{bad}");
        }
    }
}

