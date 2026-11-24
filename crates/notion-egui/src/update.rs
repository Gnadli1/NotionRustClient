//! In-app updater: fetch the newest GitHub release, verify its sha256 and
//! replace the running binary (or AppImage) in place.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;

const API: &str = "https://api.github.com/repos/Gnadli1/NotionRustClient/releases/latest";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize, Clone, Debug)]
pub struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(String),
    Downloading,
    Done(String),
    Error(String),
}

enum Msg {
    Checked(Result<Release, String>),
    Installed(Result<(), String>),
}

pub struct Updater {
    pub state: State,
    release: Option<Release>,
    rx: mpsc::Receiver<Msg>,
}

impl Default for Updater {
    fn default() -> Self {
        Updater { state: State::Idle, release: None, rx: mpsc::channel().1 }
    }
}

impl Updater {
    pub fn busy(&self) -> bool {
        matches!(self.state, State::Checking | State::Downloading)
    }

    pub fn check(&mut self) {
        if !self.busy() {
            self.state = State::Checking;
            self.spawn(|| async { Msg::Checked(latest().await) });
        }
    }

    pub fn install(&mut self) {
        if let (State::Available(_), Some(r)) = (&self.state, self.release.clone()) {
            self.state = State::Downloading;
            self.spawn(move || async move { Msg::Installed(install(&r).await) });
        }
    }

    fn spawn<F, Fut>(&mut self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Msg>,
    {
        let (tx, rx) = mpsc::channel();
        self.rx = rx;
        std::thread::spawn(move || {
            let msg = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(f()),
                Err(e) => Msg::Installed(Err(format!("tokio: {e}"))),
            };
            let _ = tx.send(msg);
        });
    }

    /// Applies a finished worker result; call every frame while `busy()`.
    pub fn poll(&mut self) {
        if !self.busy() {
            return;
        }
        let msg = match self.rx.try_recv() {
            Ok(m) => m,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(_) => Msg::Installed(Err("the update worker stopped".into())),
        };
        self.state = match msg {
            Msg::Checked(Ok(r)) if newer(CURRENT, &r.tag_name) => {
                let tag = r.tag_name.clone();
                self.release = Some(r);
                State::Available(tag)
            }
            Msg::Checked(Ok(_)) => State::UpToDate,
            Msg::Installed(Ok(())) => State::Done(self.release.as_ref().map_or_else(String::new, |r| r.tag_name.clone())),
            Msg::Checked(Err(e)) | Msg::Installed(Err(e)) => State::Error(e),
        };
    }

    pub fn tick(&self, ctx: &eframe::egui::Context) {
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }
}

/// Is release `tag` (e.g. "v0.4.0") newer than `current` ("0.3.0")? Unparsable: false.
pub fn newer(current: &str, tag: &str) -> bool {
    fn parts(s: &str) -> Option<Vec<u64>> {
        s.trim().trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect()
    }
    let (Some(mut a), Some(mut b)) = (parts(current), parts(tag)) else {
        return false;
    };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    b > a
}

/// The asset to download, its `.sha256`, and the file it replaces.
fn pick(r: &Release, appimage: Option<&str>, exe: PathBuf) -> Result<(Asset, Asset, PathBuf), String> {
    let find = |ok: &dyn Fn(&str) -> bool, what: &str| {
        r.assets
            .iter()
            .find(|a| ok(&a.name))
            .cloned()
            .ok_or_else(|| format!("this release has no {what}"))
    };
    let (bin, target) = match appimage {
        Some(p) => (find(&|n| n.ends_with("-x86_64.AppImage"), "*-x86_64.AppImage")?, PathBuf::from(p)),
        None => (find(&|n| n == "notion-egui-x86_64-linux", "notion-egui-x86_64-linux")?, exe),
    };
    let sum_name = format!("{}.sha256", bin.name);
    let sum = find(&|n| n == sum_name, &sum_name)?;
    Ok((bin, sum, target))
}

/// `sha256sum` of `file` must start with the hex digest in `expected`.
fn verify(file: &Path, expected: &str) -> Result<(), String> {
    let out = std::process::Command::new("sha256sum")
        .arg(file)
        .output()
        .map_err(|e| format!("sha256sum: {e}"))?;
    let got = String::from_utf8_lossy(&out.stdout);
    let (got, want) = (got.split_whitespace().next(), expected.split_whitespace().next());
    match (got, want) {
        (Some(g), Some(w)) if g.eq_ignore_ascii_case(w) => Ok(()),
        _ => Err("the download failed its sha256 check".into()),
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("notion-client-updater")
        .timeout(Duration::from_secs(300))
        .https_only(cfg!(not(test)))
        .build()
        .map_err(|e| e.to_string())
}

async fn latest() -> Result<Release, String> {
    client()?
        .get(API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

async fn install(r: &Release) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // trust APPIMAGE only when this process runs from inside that AppImage's mount
    let appdir = std::env::var_os("APPDIR").filter(|d| !d.is_empty());
    let appimage = std::env::var("APPIMAGE")
        .ok()
        .filter(|s| !s.is_empty() && appdir.as_ref().is_some_and(|d| exe.starts_with(d)));
    let (bin, sum, target) = pick(r, appimage.as_deref(), exe)?;
    let dir = target.parent().ok_or("no target directory")?;
    let name = target.file_name().ok_or("no target file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.new"));
    let c = client()?;
    let get = |url: String| {
        let c = c.clone();
        async move {
            c.get(url).send().await.and_then(|r| r.error_for_status())?.bytes().await
        }
    };
    if cfg!(not(test)) && ![&bin, &sum].iter().all(|a| a.browser_download_url.starts_with("https://github.com/")) {
        return Err("unexpected download URL".into());
    }
    let data = get(bin.browser_download_url).await.map_err(|e| e.to_string())?;
    let sum = get(sum.browser_download_url).await.map_err(|e| e.to_string())?;
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    if let Err(e) = std::fs::remove_file(&tmp) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("can't write {}: {e}", dir.display()));
        }
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&data))
        .map_err(|e| format!("can't write {}: {e}", dir.display()))?;
    let done = verify(&tmp, &String::from_utf8_lossy(&sum)).and_then(|()| {
        std::fs::rename(&tmp, &target).map_err(|e| format!("can't write {}: {e}", dir.display()))
    });
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"tag_name":"v0.4.0","assets":[
        {"name":"notion-egui-x86_64-linux","browser_download_url":"https://x/bin"},
        {"name":"notion-egui-x86_64-linux.sha256","browser_download_url":"https://x/bin.sha"},
        {"name":"Notion-0.4.0-x86_64.AppImage","browser_download_url":"https://x/ai"},
        {"name":"Notion-0.4.0-x86_64.AppImage.sha256","browser_download_url":"https://x/ai.sha"}]}"#;

    #[test]
    fn versions() {
        assert!(newer("0.3.0", "v0.4.0"));
        assert!(newer("0.3.0", "v0.10.0"));
        assert!(newer("0.3.0", "0.3.1"));
        assert!(!newer("0.3.0", "v0.3.0"));
        assert!(!newer("0.3.0", "v0.2.9"));
        assert!(!newer("0.3.0", "latest"));
        assert!(!newer("0.3.0", ""));
    }

    #[test]
    fn picks_assets() {
        let r: Release = serde_json::from_str(JSON).unwrap();
        let exe = PathBuf::from("/opt/notion-egui");
        let (b, s, t) = pick(&r, None, exe.clone()).unwrap();
        assert_eq!((b.browser_download_url.as_str(), s.browser_download_url.as_str()), ("https://x/bin", "https://x/bin.sha"));
        assert_eq!(t, exe);
        let (b, s, t) = pick(&r, Some("/home/me/N.AppImage"), exe.clone()).unwrap();
        assert_eq!((b.browser_download_url.as_str(), s.browser_download_url.as_str()), ("https://x/ai", "https://x/ai.sha"));
        assert_eq!(t, PathBuf::from("/home/me/N.AppImage"));
        let empty = Release { tag_name: "v1".into(), assets: vec![] };
        let e = pick(&empty, None, exe).unwrap_err();
        assert_eq!(e, "this release has no notion-egui-x86_64-linux");
    }

    #[test]
    fn sha_check() {
        let f = std::env::temp_dir().join(format!("notion-update-test-{}", std::process::id()));
        std::fs::write(&f, "hello\n").unwrap();
        let good = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03  notion-egui-x86_64-linux\n";
        assert!(verify(&f, good).is_ok());
        assert!(verify(&f, &"0".repeat(64)).is_err());
        assert!(verify(&f, "").is_err());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn version_edges() {
        assert!(!newer("0.3.0", "v0.3")); // shorter, equal
        assert!(newer("0.3", "v0.3.1"));
        assert!(newer("0.3.0", "v0.3.0.1"));
        assert!(newer("0.3.0", " v1.0.0\n"));
        assert!(!newer("0.3.0", "vv")); // no panic
        // pre-release tags do not parse -> never offered (documented behaviour)
        assert!(!newer("0.3.0", "v0.4.0-rc1"));
        assert!(!newer("0.3.0", "v0.4.0+build"));
    }

    #[test]
    fn pick_exact_names() {
        let a = |n: &str| Asset { name: n.into(), browser_download_url: format!("u/{n}") };
        let exe = PathBuf::from("/x/exe");
        // lookalikes and a sha without its binary must not match
        let r = Release { tag_name: "v1".into(), assets: vec![a("notion-egui-x86_64-linux.tar"), a("notion-egui-x86_64-linux.sha256")] };
        assert!(pick(&r, None, exe.clone()).is_err());
        // AppImage not chosen without APPIMAGE even if present; sha must match the chosen asset
        let r = Release { tag_name: "v1".into(), assets: vec![
            a("Other-x86_64.AppImage.sha256"), a("Notion_Client-1-x86_64.AppImage"), a("Notion_Client-1-x86_64.AppImage.sha256"),
            a("notion-egui-x86_64-linux"), a("notion-egui-x86_64-linux.sha256")] };
        let (b, s, t) = pick(&r, None, exe.clone()).unwrap();
        assert_eq!((b.name.as_str(), s.name.as_str(), t), ("notion-egui-x86_64-linux", "notion-egui-x86_64-linux.sha256", exe.clone()));
        let (b, s, _) = pick(&r, Some("/h/N.AppImage"), exe.clone()).unwrap();
        assert_eq!((b.name.as_str(), s.name.as_str()), ("Notion_Client-1-x86_64.AppImage", "Notion_Client-1-x86_64.AppImage.sha256"));
        // AppImage without its sha -> error naming the sha
        let r = Release { tag_name: "v1".into(), assets: vec![a("N-x86_64.AppImage")] };
        assert_eq!(pick(&r, Some("/h/N"), exe).unwrap_err(), "this release has no N-x86_64.AppImage.sha256");
    }

    /// Serves `routes` (path -> body) on 127.0.0.1 for `n` requests; returns the base URL.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>, n: usize) -> String {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for _ in 0..n {
                let (mut c, _) = l.accept().unwrap();
                let mut buf = [0u8; 4096];
                let k = c.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..k]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone());
                let head = if body.is_some() { "200 OK" } else { "404 Not Found" };
                let body = body.unwrap_or_default();
                let _ = write!(c, "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = c.write_all(&body);
            }
        });
        base
    }

    /// Local-only install test (loopback server, APPIMAGE points at a temp file).
    #[test]
    fn install_replaces_or_keeps() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("notion-install-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("N.AppImage");
        let tmp = dir.join(".N.AppImage.new");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::env::set_var("APPIMAGE", &target);
        std::env::set_var("APPDIR", "/"); // every exe path is inside "/"
        let rel = |base: &str| Release { tag_name: "v9".into(), assets: vec![
            Asset { name: "N-x86_64.AppImage".into(), browser_download_url: format!("{base}/bin") },
            Asset { name: "N-x86_64.AppImage.sha256".into(), browser_download_url: format!("{base}/sum") }] };
        let good = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03  N-x86_64.AppImage\n";

        // mismatch: original untouched, temp removed
        let base = serve(vec![("/bin", b"hello\n".to_vec()), ("/sum", "0".repeat(64).into_bytes())], 2);
        let e = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(install(&rel(&base)));
        assert_eq!(e.unwrap_err(), "the download failed its sha256 check");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!tmp.exists());

        // 404 on the binary: original untouched
        let base = serve(vec![], 1);
        let e = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(install(&rel(&base)));
        assert!(e.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");

        // success: content replaced, mode 0755, no temp left
        let base = serve(vec![("/bin", b"hello\n".to_vec()), ("/sum", good.as_bytes().to_vec())], 2);
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(install(&rel(&base))).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello\n");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!tmp.exists());
        std::env::remove_var("APPIMAGE");
        std::env::remove_var("APPDIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
