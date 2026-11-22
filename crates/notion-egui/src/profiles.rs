//! Workspace profiles: one Notion token per workspace, with a local cache and
//! session per profile. `profiles.json` lives in the config dir.
//!
//! A Notion token only reaches the workspace it belongs to. The app therefore
//! keeps one profile per workspace and switches between them; the token is
//! never printed or logged.

use std::path::{Path, PathBuf};

use notion_core::paths;
use serde::{Deserialize, Serialize};

pub const FILE: &str = "profiles.json";
pub const LEGACY_CACHE: &str = "cache.db";
pub const LEGACY_SESSION: &str = "session-egui.json";
pub const DEFAULT_NAME: &str = "Default";

/// How a profile talks to Notion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// A public-API bearer token (personal access or internal connection).
    #[default]
    Official,
    /// The private web API, using the browser's `token_v2` cookie.
    Browser,
}

/// The browser-session credential. It is account-wide, so it is stored once
/// instead of once per workspace.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub token_v2: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Full browser `Cookie` header, used only for private image/file
    /// downloads. Optional: pages and databases work with `token_v2` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
}

impl std::fmt::Debug for BrowserSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserSession")
            .field(
                "token_v2",
                &if self.token_v2.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("user_id", &self.user_id)
            .field(
                "cookies",
                &if self.cookies.as_deref().is_some_and(|c| !c.is_empty()) {
                    "<redacted>"
                } else {
                    ""
                },
            )
            .finish()
    }
}

/// One workspace. A personal access token or an internal-connection token
/// works; the token is stored in `profiles.json`, so keep that file private.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub token: String,
    /// Official public API, or the private browser-session API.
    #[serde(default)]
    pub kind: AuthKind,
    /// The workspace id for a browser-session profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// Optional: the user id used for the Updates inbox. A personal access
    /// token knows its owner, so it can stay empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Overrides the per-profile cache file name, relative to the data dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_file: Option<String>,
    /// Overrides the per-profile session file name, relative to the config dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// Built from `NOTION_TOKEN`; never written to disk.
    #[serde(skip)]
    pub from_env: bool,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field(
                "token",
                &if self.token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("space_id", &self.space_id)
            .field("user_id", &self.user_id)
            .field("cache_file", &self.cache_file)
            .field("session_file", &self.session_file)
            .finish()
    }
}

impl Profile {
    #[cfg(test)]
    pub fn new(name: impl Into<String>, token: impl Into<String>) -> Self {
        Profile {
            name: name.into(),
            token: token.into(),
            kind: AuthKind::Official,
            space_id: None,
            user_id: None,
            cache_file: None,
            session_file: None,
            from_env: false,
        }
    }

    pub fn has_token(&self) -> bool {
        !self.token.trim().is_empty()
    }

    fn cache_name(&self) -> String {
        self.cache_file
            .clone()
            .unwrap_or_else(|| format!("cache-{}.db", paths::slug(&self.name)))
    }

    /// The SQLite file of this workspace.
    pub fn cache_path(&self, data_dir: Option<&Path>) -> Option<PathBuf> {
        let file = self.cache_name();
        let path = PathBuf::from(file);
        if path.is_absolute() {
            Some(path)
        } else {
            data_dir.map(|d| d.join(path))
        }
    }

    /// The session file name of this workspace (in the config dir).
    pub fn session_file(&self) -> String {
        self.session_file
            .clone()
            .unwrap_or_else(|| format!("session-egui-{}.json", paths::slug(&self.name)))
    }
}

/// The add/edit workspace dialog's fields.
#[derive(Clone, Default)]
pub struct Form {
    /// The name before an edit; None when adding.
    pub original: Option<String>,
    pub name: String,
    pub token: String,
    pub user_id: String,
    pub error: Option<String>,
    /// Set once the dialog has taken the focus, so it does so only on opening.
    pub focused: bool,
}

impl std::fmt::Debug for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Form")
            .field("original", &self.original)
            .field("name", &self.name)
            .field(
                "token",
                &if self.token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("user_id", &self.user_id)
            .field("error", &self.error)
            .finish()
    }
}

impl Form {
    pub fn add() -> Self {
        Form::default()
    }

    pub fn edit(p: &Profile) -> Self {
        Form {
            original: Some(p.name.clone()),
            name: p.name.clone(),
            token: p.token.clone(),
            user_id: p.user_id.clone().unwrap_or_default(),
            error: None,
            focused: false,
        }
    }

    /// The profile to save. Renaming pins the old cache/session files, so the
    /// workspace keeps its data.
    pub fn to_profile(&self, before: Option<&Profile>) -> Result<Profile, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("the workspace needs a name".into());
        }
        let (cache_file, session_file) =
            match before {
                Some(old) if old.name == name => (old.cache_file.clone(), old.session_file.clone()),
                Some(old) => (
                    Some(
                        old.cache_file
                            .clone()
                            .unwrap_or_else(|| format!("cache-{}.db", paths::slug(&old.name))),
                    ),
                    Some(old.session_file.clone().unwrap_or_else(|| {
                        format!("session-egui-{}.json", paths::slug(&old.name))
                    })),
                ),
                None => (None, None),
            };
        Ok(Profile {
            name: name.to_string(),
            token: self.token.trim().to_string(),
            kind: before.map_or(AuthKind::Official, |p| p.kind),
            space_id: before.and_then(|p| p.space_id.clone()),
            user_id: Some(self.user_id.trim().to_string()).filter(|u| !u.is_empty()),
            cache_file,
            session_file,
            from_env: false,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profiles {
    pub active: Option<String>,
    pub profiles: Vec<Profile>,
    /// The browser-session cookie used by browser-mode profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserSession>,
    /// `profiles.json` exists but could not be read; `save` refuses so the
    /// original is not overwritten.
    #[serde(skip)]
    locked: bool,
}

/// Copies an unreadable `profiles.json` to `profiles.json.bad` (mode 600).
fn backup_bad(path: &Path) {
    let bad = path.with_extension("json.bad");
    if let Ok(bytes) = std::fs::read(path) {
        let _ = write_private(&bad, &bytes);
    }
}

/// Writes `bytes` to `path`, mode 600 on Unix even if the file already existed.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)?;
    f.sync_all()
}

impl Profiles {
    /// Reads `profiles.json`; a missing file falls back to `NOTION_TOKEN`
    /// without writing anything.
    pub fn load(dir: &Path) -> (Self, Option<String>) {
        Self::load_with(dir, Self::from_env())
    }

    /// `load` with the env profile passed in, so tests needn't touch the env.
    fn load_with(dir: &Path, env: Profiles) -> (Self, Option<String>) {
        let path = dir.join(FILE);
        let mut saved_active = None;
        let (mut profiles, problem) = match std::fs::read_to_string(&path) {
            Ok(src) => match serde_json::from_str::<Profiles>(&src) {
                Ok(mut p) => {
                    saved_active = p.active.clone();
                    p.normalize();
                    (p, None)
                }
                Err(e) => (
                    Profiles::default(),
                    Some(format!("{FILE}: {e}; no workspaces loaded")),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (env.clone(), None),
            Err(e) => (
                env.clone(),
                Some(format!("cannot read {}: {e}", path.display())),
            ),
        };
        if profiles.profiles.is_empty() {
            profiles = env.clone();
        }
        // the launcher's NOTION_TOKEN keeps its in-memory Default workspace
        // next to the saved ones (never persisted)
        if let Some(env) = env.profiles.into_iter().next() {
            if !profiles.profiles.iter().any(|p| p.token == env.token)
                && profiles.get(&env.name).is_none()
            {
                // save() drops the env profile but keeps `active` pointing at it
                if saved_active.as_deref() == Some(env.name.as_str()) {
                    profiles.active = saved_active;
                }
                profiles.profiles.push(env);
            }
        }
        if problem.is_some() && path.exists() {
            profiles.locked = true;
            backup_bad(&path);
        }
        (profiles, problem)
    }

    /// The old single-token mode: `NOTION_TOKEN` (and `NOTION_USER_ID`), on
    /// the shared `cache.db` and `session-egui.json`.
    pub fn from_env() -> Self {
        let token = std::env::var("NOTION_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        let Some(token) = token else {
            return Profiles::default();
        };
        let user_id = std::env::var("NOTION_USER_ID")
            .ok()
            .filter(|u| !u.trim().is_empty());
        Self::from_token(token, user_id)
    }

    fn from_token(token: String, user_id: Option<String>) -> Self {
        Profiles {
            active: Some(DEFAULT_NAME.to_string()),
            profiles: vec![Profile {
                name: DEFAULT_NAME.to_string(),
                token,
                kind: AuthKind::Official,
                space_id: None,
                user_id,
                cache_file: Some(LEGACY_CACHE.to_string()),
                session_file: Some(LEGACY_SESSION.to_string()),
                from_env: true,
            }],
            browser: None,
            locked: false,
        }
    }

    pub fn active_has_token(&self) -> bool {
        match self.active_profile() {
            Some(p) if p.kind == AuthKind::Browser => self
                .browser
                .as_ref()
                .is_some_and(|b| !b.token_v2.trim().is_empty()),
            Some(p) => p.has_token(),
            None => false,
        }
    }

    /// Adds/updates one browser profile per space. The token is stored once.
    pub fn sync_browser_spaces(
        &mut self,
        token_v2: &str,
        user_id: Option<String>,
        cookies: Option<String>,
        spaces: &[notion_core::private::Space],
    ) -> Vec<String> {
        self.browser = Some(BrowserSession {
            token_v2: token_v2.trim().to_string(),
            user_id: user_id.clone(),
            cookies,
        });
        let mut names = Vec::new();
        for space in spaces {
            let existing = self
                .profiles
                .iter()
                .find(|p| p.kind == AuthKind::Browser && p.space_id.as_deref() == Some(&space.id))
                .cloned();
            let name = existing.as_ref().map_or_else(
                || {
                    let base = if space.name.trim().is_empty() {
                        "Notion"
                    } else {
                        space.name.trim()
                    };
                    let mut name = base.to_string();
                    let mut n = 2;
                    while self.profiles.iter().any(|p| p.name == name) {
                        name = format!("{base} ({n})");
                        n += 1;
                    }
                    name
                },
                |p| p.name.clone(),
            );
            let profile = Profile {
                name: name.clone(),
                token: String::new(),
                kind: AuthKind::Browser,
                space_id: Some(space.id.clone()),
                user_id: user_id.clone(),
                cache_file: existing.as_ref().and_then(|p| p.cache_file.clone()),
                session_file: existing.as_ref().and_then(|p| p.session_file.clone()),
                from_env: false,
            };
            self.upsert(profile);
            names.push(name);
        }
        names
    }

    pub fn active_profile(&self) -> Option<&Profile> {
        let name = self.active.as_deref()?;
        self.profiles
            .iter()
            .find(|p| p.name == name)
            .or_else(|| self.profiles.first())
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn set_active(&mut self, name: &str) -> bool {
        if self.get(name).is_none() {
            return false;
        }
        self.active = Some(name.to_string());
        true
    }

    /// Replaces the profile with the same name, or adds it.
    /// A new profile whose cache/session file would collide with another's
    /// (same slug, truncated name, a renamed profile's pinned file) gets a
    /// unique pinned file stem: `slug-2`, `slug-3`, ...
    pub fn upsert(&mut self, mut profile: Profile) {
        match self.profiles.iter().position(|p| p.name == profile.name) {
            Some(i) => self.profiles[i] = profile,
            None => {
                let used = |f: fn(&Profile) -> String, name: &str| {
                    self.profiles
                        .iter()
                        .any(|p| f(p).eq_ignore_ascii_case(name))
                };
                let base = paths::slug(&profile.name);
                let mut stem = base.clone();
                let mut n = 2;
                loop {
                    let cache = format!("cache-{stem}.db");
                    let session = format!("session-egui-{stem}.json");
                    let taken = (profile.cache_file.is_none() && used(Profile::cache_name, &cache))
                        || (profile.session_file.is_none()
                            && used(Profile::session_file, &session));
                    if !taken {
                        if stem != base {
                            profile.cache_file.get_or_insert(cache);
                            profile.session_file.get_or_insert(session);
                        }
                        break;
                    }
                    stem = format!("{base}-{n}");
                    n += 1;
                }
                self.profiles.push(profile);
            }
        }
        self.normalize();
    }

    pub fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        if self.active.as_deref() == Some(name) {
            self.active = self.profiles.first().map(|p| p.name.clone());
        }
    }

    /// Trims names and tokens, drops unnamed profiles, keeps the first of
    /// duplicate names, and points `active` at a profile that exists.
    fn normalize(&mut self) {
        let mut seen = Vec::new();
        self.profiles.retain_mut(|p| {
            p.name = p.name.trim().to_string();
            p.token = p.token.trim().to_string();
            p.user_id = p
                .user_id
                .take()
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty());
            p.cache_file = p
                .cache_file
                .take()
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty());
            p.session_file = p
                .session_file
                .take()
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty());
            if p.name.is_empty() || seen.iter().any(|n| n == &p.name) {
                return false;
            }
            seen.push(p.name.clone());
            true
        });
        let active = self.active.take();
        self.active = active
            .filter(|a| self.get(a).is_some())
            .or_else(|| self.profiles.first().map(|p| p.name.clone()));
        let mut drop_browser = false;
        if let Some(b) = self.browser.as_mut() {
            b.token_v2 = b.token_v2.trim().to_string();
            b.user_id = b
                .user_id
                .take()
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty());
            drop_browser = b.token_v2.is_empty();
        }
        if drop_browser {
            self.browser = None;
        }
    }

    /// Writes `profiles.json` atomically. On Unix the file is mode 600 (dir
    /// 700): it holds the tokens. Env-token profiles are not written.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        if self.locked {
            return Err(std::io::Error::other(
                "profiles.json could not be read; fix or remove it",
            ));
        }
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)?;
        let mut out = self.clone();
        out.profiles.retain(|p| !p.from_env);
        let tmp = dir.join(format!("{FILE}.tmp"));
        write_private(&tmp, &serde_json::to_vec_pretty(&out)?)?;
        std::fs::rename(tmp, dir.join(FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_active_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Profiles::default();
        p.upsert(Profile::new(" Personal ", " ntn_a "));
        p.upsert(Profile::new("Shared", "ntn_b"));
        p.set_active("Shared");
        p.save(dir.path()).unwrap();

        let (got, problem) = Profiles::load_with(dir.path(), Profiles::default());
        assert_eq!(problem, None);
        assert_eq!(got.active.as_deref(), Some("Shared"));
        assert_eq!(got.profiles[0].token, "ntn_a");
        assert_eq!(
            got.active_profile().map(|p| p.name.as_str()),
            Some("Shared")
        );

        p.remove("Shared");
        assert_eq!(p.active.as_deref(), Some("Personal"));
    }

    #[test]
    fn bad_file_does_not_leak_the_token_in_the_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{ nope").unwrap();
        let (got, problem) = Profiles::load_with(dir.path(), Profiles::default());
        assert!(got.profiles.is_empty());
        let problem = problem.unwrap();
        assert!(problem.contains(FILE));
        assert!(!problem.contains("ntn_"));
    }

    #[test]
    fn an_old_oauth_key_is_ignored_on_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE),
            r#"{"active":"A","profiles":[{"name":"A","token":"ntn_a","workspace_id":"w","refresh_token":"r"}],
               "oauth":{"client_id":"id","client_secret":"s","redirect_uri":"http://127.0.0.1:8765/callback"}}"#,
        )
        .unwrap();
        let (got, problem) = Profiles::load_with(dir.path(), Profiles::default());
        assert_eq!(problem, None);
        assert_eq!(got.profiles[0].token, "ntn_a");
    }

    #[test]
    fn debug_redacts_the_token() {
        let p = Profile::new("x", "ntn_secret");
        let text = format!("{p:?}");
        assert!(!text.contains("ntn_secret"));
        assert!(text.contains("redacted"));
    }

    #[test]
    fn browser_spaces_create_one_profile_each_and_stay_stable() {
        let mut p = Profiles::default();
        p.upsert(Profile::new("My Notion", ""));
        let spaces = vec![
            notion_core::private::Space {
                id: "s1".into(),
                name: "Work".into(),
            },
            notion_core::private::Space {
                id: "s2".into(),
                name: "Home".into(),
            },
        ];
        let names = p.sync_browser_spaces("token", Some("u1".into()), None, &spaces);
        assert_eq!(names, ["Work", "Home"]);
        assert_eq!(p.active.as_deref(), Some("My Notion")); // the caller switches, not the sync
        assert!(p
            .profiles
            .iter()
            .all(|x| x.kind != AuthKind::Browser || x.space_id.is_some()));

        let again = p.sync_browser_spaces("token", Some("u1".into()), None, &spaces);
        assert_eq!(again, names);
        assert_eq!(
            p.profiles
                .iter()
                .filter(|x| x.kind == AuthKind::Browser)
                .count(),
            2
        );
    }

    #[test]
    fn per_profile_files_are_derived_from_the_name() {
        let p = Profile::new("My Workspace", "");
        assert_eq!(
            p.cache_path(Some(Path::new("data"))).unwrap(),
            Path::new("data/cache-my-workspace.db")
        );
        assert_eq!(p.session_file(), "session-egui-my-workspace.json");
        assert_eq!(p.cache_path(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn save_is_private_even_over_a_stale_tmp_and_creates_the_dir_700() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cfg");
        let mut p = Profiles::default();
        p.upsert(Profile::new("A", "ntn_a"));
        p.save(&dir).unwrap();
        let mode = |f: &Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&tmp, "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        p.save(&dir).unwrap();
        assert_eq!(mode(&dir.join(FILE)), 0o600);
    }

    #[test]
    fn unreadable_file_is_backed_up_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(FILE);
        std::fs::write(&f, "{ nope ntn_x").unwrap();
        let (got, problem) = Profiles::load_with(dir.path(), Profiles::default());
        assert!(problem.is_some());
        assert_eq!(
            std::fs::read(dir.path().join("profiles.json.bad")).unwrap(),
            b"{ nope ntn_x"
        );
        let err = got.save(dir.path()).unwrap_err();
        assert!(!err.to_string().contains("ntn_"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{ nope ntn_x");
    }

    #[test]
    fn load_keeps_the_env_profile_next_to_saved_ones() {
        let env = || Profiles::from_token("ntn_env".into(), None);
        let dir = tempfile::tempdir().unwrap();
        let mut p = Profiles::default();
        p.upsert(Profile::new("Work", "ntn_b"));
        p.set_active("Work");
        p.save(dir.path()).unwrap();
        let (got, _) = Profiles::load_with(dir.path(), env());
        let d = got.get(DEFAULT_NAME).expect("env profile kept");
        assert!(d.from_env && d.token == "ntn_env");
        assert_eq!(d.cache_file.as_deref(), Some(LEGACY_CACHE));
        assert_eq!(got.active.as_deref(), Some("Work"));

        // active env Default survives a save + restart
        let mut got = got;
        got.set_active(DEFAULT_NAME);
        got.save(dir.path()).unwrap();
        assert!(!std::fs::read_to_string(dir.path().join(FILE))
            .unwrap()
            .contains("ntn_env"));
        let (again, _) = Profiles::load_with(dir.path(), env());
        assert_eq!(again.active.as_deref(), Some(DEFAULT_NAME));
    }

    #[test]
    fn env_profile_token_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Profiles::default();
        p.upsert(Profile {
            from_env: true,
            ..Profile::new("Default", "ntn_envsecret")
        });
        p.upsert(Profile::new("Work", "ntn_b"));
        p.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(!text.contains("ntn_envsecret"));
        assert!(text.contains("ntn_b"));
    }

    #[test]
    fn form_debug_redacts_the_token() {
        let form = Form::edit(&Profile::new("x", "ntn_secret"));
        assert!(!format!("{form:?}").contains("ntn_secret"));
    }

    #[test]
    fn browser_session_debug_redacts_token_and_cookies() {
        let b = BrowserSession {
            token_v2: "tok_secret".into(),
            user_id: None,
            cookies: Some("c_secret=1".into()),
        };
        let t = format!("{b:?}");
        assert!(!t.contains("tok_secret") && !t.contains("c_secret"), "{t}");
    }

    #[test]
    fn colliding_names_get_distinct_files() {
        let mut p = Profiles::default();
        for n in [
            "Acme Team",
            "acme-team",
            "Acme/Team",
            &"a".repeat(60),
            &"a".repeat(70),
        ] {
            p.upsert(Profile::new(n, ""));
        }
        // a new profile reusing a renamed profile's pinned file
        let old = p.get("Acme Team").cloned().unwrap();
        let renamed = Form {
            name: "Renamed".into(),
            ..Form::edit(&old)
        }
        .to_profile(Some(&old))
        .unwrap();
        p.remove("Acme Team");
        p.upsert(renamed);
        p.upsert(Profile::new("Acme Team", ""));
        let d = Some(Path::new("d"));
        let caches: std::collections::HashSet<_> = p
            .profiles
            .iter()
            .map(|x| x.cache_path(d).unwrap())
            .collect();
        let sessions: std::collections::HashSet<_> =
            p.profiles.iter().map(|x| x.session_file()).collect();
        assert_eq!(caches.len(), p.profiles.len());
        assert_eq!(sessions.len(), p.profiles.len());
    }
}
