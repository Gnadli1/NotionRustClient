//! Browser-session login: paste the `token_v2` cookie from a logged-in
//! browser, discover every workspace it can see, and create one native
//! profile per workspace.
//!
//! The cookie is treated like a password: it is stored only in
//! `profiles.json`, never logged, and can be replaced or cleared there.

use std::sync::mpsc;
use std::time::Duration;

use notion_core::private::Space;

type Spaces = Result<(Option<String>, Vec<Space>), String>;

pub struct Flow {
    pub token: String,
    pub cookies: String,
    pub error: Option<String>,
    pub busy: bool,
    rx: mpsc::Receiver<Spaces>,
}

impl Flow {
    pub fn new(prefill: &str, cookies: &str) -> Self {
        let (_, rx) = mpsc::channel();
        Flow {
            token: prefill.to_string(),
            cookies: cookies.to_string(),
            error: None,
            busy: false,
            rx,
        }
    }

    /// Starts the discovery request. The token is read on the worker thread,
    /// so a slow login never blocks the UI.
    pub fn connect(&mut self) {
        if self.busy {
            return;
        }
        self.cookies = normalize_header(&self.cookies);
        let mut token = token_from_input(&self.token);
        self.token = token.clone();
        if token.is_empty() {
            token = cookie_value(&self.cookies, "token_v2").unwrap_or_default();
            self.token = token.clone();
        }
        if token.is_empty() {
            self.error = Some("Paste token_v2, or the full Cookie header from notion.so.".into());
            return;
        }
        self.error = None;
        self.busy = true;
        let (tx, rx) = mpsc::channel();
        self.rx = rx;
        std::thread::spawn(move || {
            let base = std::env::var("NOTION_PRIVATE_BASE")
                .ok()
                .filter(|b| notion_core::private::safe_base(b))
                .unwrap_or_else(|| notion_core::private::DEFAULT_BASE.to_string());
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("tokio: {e}"))
                .and_then(|rt| {
                    rt.block_on(notion_core::private::browser_spaces(&token, &base))
                        .map_err(|e| e.to_string())
                });
            let _ = tx.send(result);
        });
    }

    pub fn poll(&mut self) -> Option<Spaces> {
        if !self.busy {
            return None;
        }
        match self.rx.try_recv() {
            Ok(result) => {
                self.busy = false;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = false;
                Some(Err("the login worker stopped".into()))
            }
        }
    }

    pub fn tick(&self, ctx: &eframe::egui::Context) {
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }
}

/// Accepts both a bare `name=value; ...` header and a pasted `Cookie:` line.
pub fn normalize_header(s: &str) -> String {
    s.trim()
        .strip_prefix("Cookie:")
        .unwrap_or(s.trim())
        .trim()
        .to_string()
}

/// The value of cookie `name` in a `name=value; ...` header.
fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies.split(';').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// Accepts a bare token, `token_v2=…`, or a pasted `Cookie:` header.
pub fn token_from_input(input: &str) -> String {
    let header = normalize_header(input);
    if header.contains("token_v2=") {
        cookie_value(&header, "token_v2").unwrap_or(header)
    } else {
        header
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::{token_from_input, Flow, Spaces};

    /// A finished login whose discovery answered `result`.
    pub(crate) fn done(token: &str, result: Spaces) -> Flow {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(result).unwrap();
        Flow {
            token: token.into(),
            cookies: String::new(),
            error: None,
            busy: true,
            rx,
        }
    }

    #[test]
    fn accepts_a_bare_token_and_a_cookie_header() {
        assert_eq!(token_from_input("  abc  "), "abc");
        assert_eq!(token_from_input("token_v2=abc"), "abc");
        assert_eq!(
            token_from_input("Cookie: notion_user_id=u; token_v2=abc; x=y"),
            "abc"
        );
    }
}
