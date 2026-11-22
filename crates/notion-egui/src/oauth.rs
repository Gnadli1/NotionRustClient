//! "Sign in with Notion": the desktop side of the OAuth flow.
//!
//! A local loopback listener catches Notion's redirect, verifies `state`, and
//! exchanges the code for a token. If the redirect URI is not a local HTTP
//! address, the user can paste the redirected URL (or just the code) instead.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use notion_core::oauth::{self, TokenResponse};

use crate::profiles::OAuthApp;

const TIMEOUT: Duration = Duration::from_secs(600);

type Answer = Result<TokenResponse, String>;

/// One in-flight sign-in.
pub struct Flow {
    pub app: OAuthApp,
    pub auth_url: String,
    /// The redirected URL or the bare code, for a non-local redirect URI.
    pub manual: String,
    pub error: Option<String>,
    pub busy: bool,
    rx: mpsc::Receiver<Answer>,
    tx: mpsc::Sender<Answer>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

impl Drop for Flow {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Flow {
    /// Starts the listener (when the redirect is local) and builds the URL.
    /// The caller opens `auth_url` in the browser.
    pub fn start(app: OAuthApp) -> Result<Flow, String> {
        if !app.is_configured() {
            return Err("the OAuth client id and secret are missing".into());
        }
        let state = uuid::Uuid::new_v4().to_string();
        let auth_url = oauth::authorize_url(&app.client_id, &app.redirect_uri, &state);
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(port) = local_port(&app.redirect_uri) {
            let mut started = false;
            let mut last_error = None;
            // `localhost` may resolve to IPv6: listen on both loopbacks.
            for host in ["127.0.0.1", "::1"] {
                match TcpListener::bind((host, port)) {
                    Ok(listener) => {
                        if let Err(e) = listener.set_nonblocking(true) {
                            last_error = Some(e.to_string());
                            continue;
                        }
                        started = true;
                        let answer = tx.clone();
                        let cancel = cancel.clone();
                        let flow_app = app.clone();
                        let state = state.clone();
                        std::thread::spawn(move || {
                            listen(listener, flow_app, state, answer, cancel);
                        });
                    }
                    Err(e) => last_error = Some(e.to_string()),
                }
            }
            if !started {
                return Err(format!(
                    "cannot listen on port {port}: {}. Change the redirect URI and paste the code instead.",
                    last_error.unwrap_or_else(|| "no loopback address".into())
                ));
            }
        }
        Ok(Flow {
            app,
            auth_url,
            manual: String::new(),
            error: None,
            busy: false,
            rx,
            tx,
            cancel,
            started: Instant::now(),
        })
    }

    /// The token, or an error. None means keep waiting.
    pub fn poll(&mut self) -> Option<Answer> {
        match self.rx.try_recv() {
            Ok(r) => {
                self.busy = false;
                Some(r)
            }
            Err(mpsc::TryRecvError::Empty) => {
                if self.started.elapsed() > TIMEOUT {
                    Some(Err("Notion sign-in timed out".into()))
                } else {
                    None
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Notion sign-in stopped".into())),
        }
    }

    pub fn open_again(&self) -> Result<(), String> {
        webbrowser::open(&self.auth_url)
            .map(|_| ())
            .map_err(|e| format!("cannot open the browser: {e}"))
    }

    /// Exchanges a pasted redirect URL or bare code.
    pub fn submit_manual(&mut self) {
        if self.busy {
            return;
        }
        let Some(code) = code_from_input(&self.manual) else {
            self.error = Some("Paste the URL Notion redirected to, or the code value.".into());
            return;
        };
        self.error = None;
        self.busy = true;
        let app = self.app.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(exchange(&app, &code));
        });
    }
}

fn local_port(uri: &str) -> Option<u16> {
    let rest = uri.strip_prefix("http://")?;
    let hostport = rest.split('/').next()?;
    let (host, port) = hostport.rsplit_once(':')?;
    matches!(host, "127.0.0.1" | "localhost")
        .then(|| port.parse().ok())
        .flatten()
}

fn listen(
    listener: TcpListener,
    app: OAuthApp,
    state: String,
    tx: mpsc::Sender<Answer>,
    cancel: Arc<AtomicBool>,
) {
    let deadline = Instant::now() + TIMEOUT;
    let mut buf = [0u8; 8192];
    loop {
        if cancel.load(Ordering::Relaxed) || Instant::now() > deadline {
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1));
                let Some(target) = target else { continue };
                match callback(target, &state) {
                    Some(Err(e)) => {
                        reply(&mut stream, 400, "Notion sign-in failed", &e);
                        let _ = tx.send(Err(e));
                        return;
                    }
                    Some(Ok(code)) => {
                        let result = exchange(&app, &code);
                        let ok = result.is_ok();
                        let body = result
                            .as_ref()
                            .map(|_| "You are signed in. Close this tab and return to the app.")
                            .unwrap_or_else(|e| e.as_str())
                            .to_string();
                        reply(
                            &mut stream,
                            if ok { 200 } else { 400 },
                            if ok { "Signed in" } else { "Sign-in failed" },
                            &body,
                        );
                        let _ = tx.send(result);
                        return;
                    }
                    None => {
                        reply(
                            &mut stream,
                            404,
                            "Waiting",
                            "Waiting for Notion's redirect.",
                        );
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = tx.send(Err(format!("local sign-in listener: {e}")));
                return;
            }
        }
    }
}

fn exchange(app: &OAuthApp, code: &str) -> Answer {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    rt.block_on(oauth::exchange_code(
        &app.client_id,
        &app.client_secret,
        &app.redirect_uri,
        code,
    ))
    .map_err(|e| e.to_string())
}

fn reply(stream: &mut TcpStream, status: u16, title: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font-family:system-ui;max-width:36rem;margin:4rem auto\">\
         <h1>{title}</h1><p>{body}</p></body>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
}

/// The code and state in a local callback URL, or an error from Notion. None
/// means this request is not a callback (favicon, etc.).
fn callback(target: &str, expected_state: &str) -> Option<Result<String, String>> {
    let query = target.split_once('?')?.1;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            "error_description" => description = Some(v),
            _ => {}
        }
    }
    if let Some(e) = error {
        return Some(Err(description.unwrap_or(e)));
    }
    let code = code?;
    if state.as_deref() != Some(expected_state) {
        return Some(Err(
            "the sign-in response had the wrong state; try again".into()
        ));
    }
    Some(Ok(code))
}

/// A pasted callback URL, or a bare code.
pub fn code_from_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some(i) = input.find("code=") {
        let rest = &input[i + 5..];
        let code = rest.split(['&', '#', ' ']).next()?;
        let code = decode(code);
        return (!code.is_empty()).then_some(code);
    }
    // A bare code is a UUID-like token; reject a whole URL without ?code=.
    if input.contains("://") || input.contains('/') || input.contains('?') {
        return None;
    }
    Some(input.to_string())
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_needs_the_right_state() {
        assert_eq!(
            callback("/callback?code=abc&state=s1", "s1"),
            Some(Ok("abc".into()))
        );
        assert!(matches!(
            callback("/callback?code=abc&state=bad", "s1"),
            Some(Err(_))
        ));
        assert!(matches!(
            callback("/callback?error=access_denied&error_description=no", "s1"),
            Some(Err(e)) if e == "no"
        ));
        assert_eq!(callback("/favicon.ico", "s1"), None);
    }

    #[test]
    fn pasted_url_or_bare_code() {
        assert_eq!(
            code_from_input("http://127.0.0.1:8765/callback?code=abc%2D1&state=x"),
            Some("abc-1".into())
        );
        assert_eq!(code_from_input("abc-1"), Some("abc-1".into()));
        assert_eq!(code_from_input("https://example.com/callback"), None);
        assert_eq!(code_from_input(""), None);
    }

    #[test]
    fn only_http_loopback_is_caught_locally() {
        assert_eq!(local_port("http://127.0.0.1:8765/callback"), Some(8765));
        assert_eq!(local_port("http://localhost:9000/cb"), Some(9000));
        assert_eq!(local_port("https://localhost:8765/callback"), None);
        assert_eq!(local_port("https://example.com/callback"), None);
    }
}
