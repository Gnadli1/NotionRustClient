//! Where a click on a link goes (ported from notion-gui's app.rs): a Notion
//! page opens in the app, other http/https links go to the system browser,
//! anything else is refused. Pure functions, so tests never start a process.

use eframe::egui::text::LayoutJob;
use notion_core::model::{RichText, RichTextKind};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Link {
    /// A page id to open in the app (pulled if uncached).
    Page(String),
    /// A URL for the opener.
    Web(String),
}

/// The page id (8-4-4-4-12) a Notion link points at: a notion.so /
/// www.notion.so / notion.com / www.notion.com / *.notion.site URL, or a
/// relative `/…` path, whose last segment ends in a 32-hex id (dashed or not;
/// `?…#…` ignored).
pub(crate) fn notion_page_id(url: &str) -> Option<String> {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let path = if url.starts_with('/') {
        url
    } else {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let (host, path) = rest.split_at(rest.find('/')?);
        let host = host.to_ascii_lowercase();
        let hosts = ["notion.so", "www.notion.so", "notion.com", "www.notion.com"];
        if !(hosts.contains(&host.as_str()) || host.ends_with(".notion.site")) {
            return None;
        }
        path
    };
    let last: String = path
        .rsplit('/')
        .next()?
        .chars()
        .filter(|&c| c != '-')
        .collect();
    let h = last
        .get(last.len().checked_sub(32)?..)?
        .to_ascii_lowercase();
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// A clicked URL: a Notion page opens in the app, anything else goes out.
pub(crate) fn route(url: &str) -> Link {
    match notion_page_id(url) {
        Some(id) => Link::Page(id),
        None => Link::Web(url.to_string()),
    }
}

/// What clicking a run does: a page mention opens the page, a link is routed.
pub(crate) fn run_link(r: &RichText) -> Option<Link> {
    match &r.kind {
        RichTextKind::Mention(m) if m["type"] == "page" => m["page"]["id"]
            .as_str()
            .map(|id| Link::Page(id.to_string())),
        _ => r.href.as_deref().map(route),
    }
}

/// The link under char `cursor` of the label `job` (`rich_job`: one section
/// per run, in order).
pub(crate) fn link_at(rich: &[RichText], job: &LayoutJob, cursor: usize) -> Option<Link> {
    let byte = job
        .text
        .char_indices()
        .nth(cursor)
        .map_or(job.text.len(), |(b, _)| b);
    job.sections
        .iter()
        .zip(rich)
        .find(|(s, _)| (s.byte_range.start.0..s.byte_range.end.0).contains(&byte))
        .and_then(|(_, r)| run_link(r))
}

/// Only web links go to the browser (a file:// one could start a local script
/// or .desktop file).
pub(crate) fn allowed_url(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    ["http://", "https://"].iter().any(|p| l.starts_with(p))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Opens `url` in the system browser (ShellExecute on Windows, `open` on
/// macOS, `xdg-open` on Linux) without waiting for it.
pub(crate) fn open_url(url: &str) -> Result<(), String> {
    webbrowser::open(url)
        .map(|_| ())
        .map_err(|e| format!("cannot open the browser: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md::rich_job;
    use serde_json::json;

    const PID: &str = "0123abcd-0123-4567-89ab-0123456789ab";

    #[test]
    fn notion_urls_parse_to_page_ids() {
        let hex = "0123abcd0123456789ab0123456789ab";
        for url in [
            format!("https://www.notion.so/{hex}"),
            format!("https://notion.so/{PID}"),
            format!("https://www.notion.so/team/My-Page-{hex}?pvs=4"),
            format!("http://WWW.Notion.so/My-Page-{}#abc", hex.to_uppercase()),
            format!("https://acme.notion.site/Page-{hex}"),
            format!("https://www.notion.com/Page-{hex}"),
            format!("https://notion.com/{PID}?pvs=4"),
            format!("/{hex}"),
            format!("/My-Page-{PID}?v=1#x"),
        ] {
            assert_eq!(notion_page_id(&url).as_deref(), Some(PID), "{url}");
        }
        for url in [
            format!("https://example.com/{hex}"),
            format!("https://notion.so.evil.com/{hex}"),
            format!("https://fakenotion.site/{hex}"),
            format!("https://notion.com.evil.com/{hex}"),
            "https://www.notion.so/no-id-here".into(),
            format!("https://www.notion.so/{hex}/sub"),
            format!("file:///{hex}"),
            format!("https://www.notion.so/x?p={hex}"),
            "/short-abc".into(),
        ] {
            assert_eq!(notion_page_id(&url), None, "{url}");
        }
    }

    #[test]
    fn routing_and_the_scheme_whitelist() {
        assert_eq!(
            route(&format!("https://notion.so/{PID}")),
            Link::Page(PID.into())
        );
        assert_eq!(
            route("https://example.com/a"),
            Link::Web("https://example.com/a".into())
        );
        assert!(allowed_url("HTTPS://x.example"));
        assert!(allowed_url("http://x.example"));
        for bad in [
            "file:///etc/passwd",
            "mailto:x@y",
            "javascript:1",
            "x.example",
            "https://x.example/a b",
            "https://x.example/\n--flag",
            "https://x.example/\u{0}",
            "https://x.example/\u{a0}",
        ] {
            assert!(!allowed_url(bad), "{bad}");
        }
    }

    fn run(v: serde_json::Value) -> RichText {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_click_finds_its_run_even_with_an_equation_in_front() {
        let rich = vec![
            run(
                json!({ "type": "equation", "equation": { "expression": "\\alpha" },
                "plain_text": "\\alpha", "annotations": {} }),
            ),
            run(
                json!({ "type": "text", "text": { "content": "go", "link": { "url": "https://e.x/" } },
                "plain_text": "go", "href": "https://e.x/", "annotations": {} }),
            ),
            run(
                json!({ "type": "mention", "mention": { "type": "page", "page": { "id": PID } },
                "plain_text": "pg", "annotations": {} }),
            ),
        ];
        let job = rich_job(&rich, 16.0, false, &[]);
        // the equation shows as "α" (1 char), not as its 6-char source
        assert_eq!(job.text.chars().count(), 1 + 2 + 2, "{}", job.text);
        assert_eq!(link_at(&rich, &job, 0), None);
        assert_eq!(
            link_at(&rich, &job, 1),
            Some(Link::Web("https://e.x/".into()))
        );
        assert_eq!(link_at(&rich, &job, 3), Some(Link::Page(PID.into())));
        assert_eq!(link_at(&rich, &job, 99), None);
    }
}
