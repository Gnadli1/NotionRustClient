//! Where the cache and the config live, on Linux, macOS and Windows.

use std::path::PathBuf;

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_DATA_HOME/notion-client`, `~/Library/Application Support/notion-client`
/// on macOS, `%LOCALAPPDATA%\notion-client` on Windows.
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(d) = var("LOCALAPPDATA").or_else(|| var("APPDATA")) {
            return Some(d.join("notion-client"));
        }
        if let Some(h) = var("USERPROFILE") {
            return Some(h.join("AppData").join("Local").join("notion-client"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = var("HOME") {
            return Some(
                h.join("Library")
                    .join("Application Support")
                    .join("notion-client"),
            );
        }
    }
    if let Some(d) = var("XDG_DATA_HOME") {
        return Some(d.join("notion-client"));
    }
    var("HOME").map(|h| h.join(".local").join("share").join("notion-client"))
}

/// `$XDG_CONFIG_HOME/notion-client`, `~/Library/Application Support/notion-client`
/// on macOS, `%APPDATA%\notion-client` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(d) = var("APPDATA").or_else(|| var("LOCALAPPDATA")) {
            return Some(d.join("notion-client"));
        }
        if let Some(h) = var("USERPROFILE") {
            return Some(h.join("AppData").join("Roaming").join("notion-client"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = var("HOME") {
            return Some(
                h.join("Library")
                    .join("Application Support")
                    .join("notion-client"),
            );
        }
    }
    if let Some(d) = var("XDG_CONFIG_HOME") {
        return Some(d.join("notion-client"));
    }
    var("HOME").map(|h| h.join(".config").join("notion-client"))
}

/// The single-cache location shared by the app and the CLI.
pub fn default_db() -> Option<PathBuf> {
    data_dir().map(|d| d.join("cache.db"))
}

/// A safe file-name stem for a workspace name: lower-case letters, digits and
/// `-`, at most 48 characters, never empty.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.chars().count() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "workspace".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_safe_and_stable() {
        assert_eq!(slug("My Notion"), "my-notion");
        assert_eq!(slug("  Acme / Team  "), "acme-team");
        assert_eq!(slug("München"), "münchen");
        assert_eq!(slug("***"), "workspace");
        assert_eq!(slug(""), "workspace");
        assert!(slug(&"a".repeat(100)).chars().count() <= 48);
    }
}
