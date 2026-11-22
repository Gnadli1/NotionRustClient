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

/// The OAuth app the user creates once in Notion's Developer portal. Its
/// client secret is not a workspace key; it authorizes the login flow itself.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect")]
    pub redirect_uri: String,
}

fn default_redirect() -> String {
    notion_core::oauth::DEFAULT_REDIRECT.to_string()
}

impl Default for OAuthApp {
    fn default() -> Self {
        OAuthApp {
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: default_redirect(),
        }
    }
}

impl std::fmt::Debug for OAuthApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthApp")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &if self.client_secret.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

impl OAuthApp {
    pub fn is_configured(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.trim().is_empty()
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
    /// Set for an OAuth login: the workspace it was authorized in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Set for an OAuth login; lets the app refresh the access token later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
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
            .field("workspace_id", &self.workspace_id)
            .field("oauth", &self.refresh_token.is_some())
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
            workspace_id: None,
            refresh_token: None,
        }
    }

    pub fn has_token(&self) -> bool {
        !self.token.trim().is_empty()
    }

    /// The SQLite file of this workspace.
    pub fn cache_path(&self, data_dir: Option<&Path>) -> Option<PathBuf> {
        let file = self
            .cache_file
            .clone()
            .unwrap_or_else(|| format!("cache-{}.db", paths::slug(&self.name)));
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
#[derive(Clone, Debug, Default)]
pub struct Form {
    /// The name before an edit; None when adding.
    pub original: Option<String>,
    pub name: String,
    pub token: String,
    pub user_id: String,
    pub error: Option<String>,
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
            workspace_id: before.and_then(|p| p.workspace_id.clone()),
            refresh_token: before.and_then(|p| p.refresh_token.clone()),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profiles {
    pub active: Option<String>,
    pub profiles: Vec<Profile>,
    /// The public OAuth integration used by "Sign in with Notion".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthApp>,
    /// The browser-session cookie used by browser-mode profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserSession>,
}

impl Profiles {
    /// Reads `profiles.json`; a missing file falls back to `NOTION_TOKEN`
    /// without writing anything.
    pub fn load(dir: &Path) -> (Self, Option<String>) {
        let path = dir.join(FILE);
        let (mut profiles, problem) = match std::fs::read_to_string(&path) {
            Ok(src) => match serde_json::from_str::<Profiles>(&src) {
                Ok(mut p) => {
                    p.normalize();
                    (p, None)
                }
                Err(e) => (
                    Profiles::default(),
                    Some(format!("{FILE}: {e}; no workspaces loaded")),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::from_env(), None),
            Err(e) => (
                Self::from_env(),
                Some(format!("cannot read {}: {e}", path.display())),
            ),
        };
        if profiles.profiles.is_empty() {
            profiles = Self::from_env();
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
                workspace_id: None,
                refresh_token: None,
            }],
            oauth: None,
            browser: None,
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
                workspace_id: None,
                refresh_token: None,
            };
            self.upsert(profile);
            names.push(name);
        }
        if self.active_profile().map(|p| p.kind) != Some(AuthKind::Browser) {
            if let Some(first) = names.first() {
                self.set_active(first);
            }
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
    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(p) => *p = profile,
            None => self.profiles.push(profile),
        }
        self.normalize();
    }

    pub fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        if self.active.as_deref() == Some(name) {
            self.active = self.profiles.first().map(|p| p.name.clone());
        }
    }

    /// Adds or updates the profile from an OAuth token response. An existing
    /// profile for the same workspace id keeps its cache and session; a new
    /// workspace gets a unique name.
    pub fn upsert_oauth(
        &mut self,
        token: &notion_core::oauth::TokenResponse,
        fallback: &str,
    ) -> Profile {
        let base = token.workspace().unwrap_or(fallback).to_string();
        let existing = token
            .workspace_id
            .as_deref()
            .and_then(|id| {
                self.profiles
                    .iter()
                    .find(|p| p.workspace_id.as_deref() == Some(id))
            })
            .cloned()
            .or_else(|| self.profiles.iter().find(|p| p.name == base).cloned());
        let name = existing.as_ref().map_or_else(
            || {
                let mut name = base.clone();
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
            token: token.access_token.clone(),
            kind: AuthKind::Official,
            space_id: None,
            user_id: token.user_id(),
            cache_file: existing.as_ref().and_then(|p| p.cache_file.clone()),
            session_file: existing.as_ref().and_then(|p| p.session_file.clone()),
            workspace_id: token.workspace_id.clone(),
            refresh_token: token.refresh_token.clone(),
        };
        self.upsert(profile.clone());
        self.set_active(&name);
        profile
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
            p.workspace_id = p
                .workspace_id
                .take()
                .map(|w| w.trim().to_string())
                .filter(|w| !w.is_empty());
            p.refresh_token = p
                .refresh_token
                .take()
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty());
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
        let mut drop_oauth = false;
        if let Some(o) = self.oauth.as_mut() {
            o.client_id = o.client_id.trim().to_string();
            o.client_secret = o.client_secret.trim().to_string();
            o.redirect_uri = o.redirect_uri.trim().to_string();
            if o.redirect_uri.is_empty() {
                o.redirect_uri = default_redirect();
            }
            drop_oauth = o.client_id.is_empty() && o.client_secret.is_empty();
        }
        if drop_oauth {
            self.oauth = None;
        }
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

    /// Writes `profiles.json` atomically. On Unix the file is mode 600: it
    /// holds the tokens.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
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
        p.oauth = Some(OAuthApp {
            client_id: " id ".into(),
            client_secret: " secret ".into(),
            redirect_uri: " http://127.0.0.1:8765/callback ".into(),
        });
        p.save(dir.path()).unwrap();

        let (got, problem) = Profiles::load(dir.path());
        assert_eq!(problem, None);
        assert_eq!(got.active.as_deref(), Some("Shared"));
        assert_eq!(got.profiles[0].token, "ntn_a");
        assert_eq!(
            got.active_profile().map(|p| p.name.as_str()),
            Some("Shared")
        );
        let oauth = got.oauth.as_ref().unwrap();
        assert_eq!(
            (oauth.client_id.as_str(), oauth.client_secret.as_str()),
            ("id", "secret")
        );

        p.remove("Shared");
        assert_eq!(p.active.as_deref(), Some("Personal"));
    }

    #[test]
    fn bad_file_does_not_leak_the_token_in_the_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{ nope").unwrap();
        let (got, problem) = Profiles::load(dir.path());
        assert!(got.profiles.is_empty());
        let problem = problem.unwrap();
        assert!(problem.contains(FILE));
        assert!(!problem.contains("ntn_"));
    }

    #[test]
    fn debug_redacts_the_token() {
        let p = Profile::new("x", "ntn_secret");
        let text = format!("{p:?}");
        assert!(!text.contains("ntn_secret"));
        assert!(text.contains("redacted"));
    }

    #[test]
    fn an_oauth_login_creates_then_updates_one_profile() {
        let mut p = Profiles::default();
        let first: notion_core::oauth::TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "a1",
            "refresh_token": "r1",
            "workspace_id": "w1",
            "workspace_name": "Acme",
            "owner": { "type": "user", "user": { "id": "u1" } }
        }))
        .unwrap();
        let made = p.upsert_oauth(&first, "fallback");
        assert_eq!(made.name, "Acme");
        assert_eq!(made.token, "a1");
        assert_eq!(made.user_id.as_deref(), Some("u1"));
        assert_eq!(made.workspace_id.as_deref(), Some("w1"));
        assert_eq!(p.active.as_deref(), Some("Acme"));

        let again: notion_core::oauth::TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "a2",
            "refresh_token": "r2",
            "workspace_id": "w1",
            "workspace_name": "Acme"
        }))
        .unwrap();
        p.upsert_oauth(&again, "fallback");
        assert_eq!(p.profiles.len(), 1, "same workspace id reuses the profile");
        assert_eq!(p.profiles[0].token, "a2");
        assert_eq!(p.profiles[0].refresh_token.as_deref(), Some("r2"));
    }

    #[test]
    fn oauth_client_secret_is_redacted_in_debug() {
        let app = OAuthApp {
            client_id: "id".into(),
            client_secret: "super-secret".into(),
            redirect_uri: default_redirect(),
        };
        let text = format!("{app:?}");
        assert!(!text.contains("super-secret"));
        assert!(text.contains("redacted"));
        assert!(app.is_configured());
    }

    #[test]
    fn browser_spaces_create_one_profile_each_and_stay_stable() {
        let mut p = Profiles::default();
        p.upsert(Profile::new("My Notion", ""));
        let spaces = vec![
            notion_core::private::Space {
                id: "s1".into(),
                name: "Nicolas's Notion".into(),
            },
            notion_core::private::Space {
                id: "s2".into(),
                name: "Paul's Notion".into(),
            },
        ];
        let names = p.sync_browser_spaces("token", Some("u1".into()), None, &spaces);
        assert_eq!(names, ["Nicolas's Notion", "Paul's Notion"]);
        assert_eq!(p.active.as_deref(), Some("Nicolas's Notion"));
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
}
