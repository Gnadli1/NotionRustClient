//! LayoutJobs: read-only rich text, and live Markdown styling of the row being
//! edited (markers stay in the text, dimmed). Colours are `PLACEHOLDER`, so the
//! widget's own text colour (light or dark theme) applies.

use eframe::egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontFamily, FontId, Stroke,
};
use notion_core::model::{RichText, RichTextKind, User};

use crate::tex::pretty_tex;

const LINK: Color32 = Color32::from_rgb(70, 130, 220);
const CODE_BG: Color32 = Color32::from_rgba_premultiplied(40, 40, 40, 40);
/// A mention's pill.
const MENTION_BG: Color32 = Color32::from_rgba_premultiplied(30, 50, 80, 50);

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub underline: bool,
    pub link: bool,
    pub math: bool,
    /// A mention's label: a pill.
    pub mention: bool,
}

/// Registered in `main::setup_fonts` (falls back to the normal face).
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn format(size: f32, s: Style) -> TextFormat {
    let (family, size) = if s.code {
        (FontFamily::Monospace, size * 0.9)
    } else if s.bold {
        (bold_family(), size)
    } else {
        (FontFamily::Proportional, size)
    };
    TextFormat {
        font_id: FontId::new(size, family),
        color: if s.link { LINK } else { Color32::PLACEHOLDER },
        background: if s.code {
            CODE_BG
        } else if s.mention {
            MENTION_BG
        } else {
            Color32::TRANSPARENT
        },
        italics: s.italic || s.math,
        underline: if s.underline || s.link {
            Stroke::new(1.0, Color32::PLACEHOLDER)
        } else {
            Stroke::NONE
        },
        strikethrough: if s.strike {
            Stroke::new(1.0, Color32::PLACEHOLDER)
        } else {
            Stroke::NONE
        },
        ..Default::default()
    }
}

/// "@Name" for a user mention: the cached name, else what the API said
/// (`plain_text`, "@" or not), else the id's first 8 characters.
fn user_mention_text(m: &serde_json::Value, plain: &str, users: &[User]) -> Option<String> {
    if m["type"] != "user" {
        return None;
    }
    let id = m["user"]["id"].as_str().unwrap_or_default();
    let name = match crate::people::find(users, id).filter(|u| !u.name.is_empty()) {
        Some(u) => u.name.clone(),
        None => match plain.trim_start_matches('@') {
            "" => id.chars().take(8).collect(),
            p => p.to_string(),
        },
    };
    Some(format!("@{name}"))
}

/// A cached row's rich text. `bold` is the row's base weight (headings);
/// `users` name the user mentions.
pub fn rich_job(rt: &[RichText], size: f32, bold: bool, users: &[User]) -> LayoutJob {
    let mut job = LayoutJob::default();
    for r in rt {
        let a = &r.annotations;
        let mut s = Style {
            bold: bold || a.bold,
            italic: a.italic,
            code: a.code,
            strike: a.strikethrough,
            underline: a.underline,
            link: r.href.is_some(),
            math: false,
            mention: matches!(r.kind, RichTextKind::Mention(_)),
        };
        let text = match &r.kind {
            RichTextKind::Equation { expression } => {
                s.math = true;
                pretty_tex(expression).unwrap_or_else(|| expression.clone())
            }
            RichTextKind::Mention(m) => {
                user_mention_text(m, &r.plain_text, users).unwrap_or_else(|| r.plain_text.clone())
            }
            _ => r.plain_text.clone(),
        };
        job.append(&text, 0.0, format(size, s));
    }
    if job.is_empty() {
        job.append(" ", 0.0, format(size, Style::default())); // keeps the row's height
    }
    job
}

/// For `@[label](kind:id)` at the start of `s`: the byte where the label ends
/// (at its `]`) and where the token ends. Backslash escapes the next char.
fn mention_span(s: &str) -> Option<(usize, usize)> {
    let mut it = s.char_indices().skip(2);
    let close = loop {
        match it.next()? {
            (_, '\\') => {
                it.next();
            }
            (i, ']') => break i,
            _ => {}
        }
    };
    s[close..].starts_with("](").then_some(())?;
    let mut it = s[close + 2..].char_indices();
    loop {
        match it.next()? {
            (_, '\\') => {
                it.next();
            }
            (i, ')') => return Some((close, close + 2 + i + 1)),
            _ => {}
        }
    }
}

fn flag<'a>(s: &'a mut Style, d: &str) -> &'a mut bool {
    match d {
        "**" => &mut s.bold,
        "~~" => &mut s.strike,
        "*" => &mut s.italic,
        "`" => &mut s.code,
        _ => &mut s.math,
    }
}

/// The Markdown source styled as it would render; `job.text == src`.
/// An opener only counts if its closer follows and no space comes right after
/// it, so `2 * 3` stays plain.
/// `bold` is the row's base weight (headings).
pub fn md_job(src: &str, size: f32, bold: bool, dim: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut st = Style::default();
    let (mut start, mut i) = (0, 0);
    let with_base = |st: Style| Style {
        bold: st.bold || bold,
        ..st
    };
    // src[a..b] in style `st`, or dimmed when it is a marker
    let put = |job: &mut LayoutJob, st: Style, a: usize, b: usize, marker: bool| {
        if a < b {
            let mut f = format(size, with_base(st));
            if marker {
                f.color = dim;
            }
            job.append(&src[a..b], 0.0, f);
        }
    };
    while i < src.len() {
        let rest = &src[i..];
        // inside `code` or $math$ only the closer is special
        let raw = if st.code {
            Some('`')
        } else if st.math {
            Some('$')
        } else {
            None
        };
        let delim = match raw {
            Some(c) => rest.starts_with(c).then(|| &rest[..1]),
            None => ["**", "~~", "*", "`", "$"]
                .into_iter()
                .find(|d| rest.starts_with(d)),
        };
        if raw.is_none() && rest.starts_with("@[") {
            // @[label](kind:id): the label is a pill, the rest stays visible, dimmed
            if let Some((label, end)) = mention_span(rest) {
                put(&mut job, st, start, i, false);
                put(&mut job, st, i, i + 2, true);
                put(&mut job, Style { mention: true, ..st }, i + 2, i + label, false);
                put(&mut job, st, i + label, i + end, true);
                i += end;
                start = i;
                continue;
            }
        }
        if raw.is_none() && rest.starts_with('[') {
            // [label](url)
            if let Some(end) = rest
                .find("](")
                .and_then(|mid| rest[mid..].find(')').map(|e| (mid, mid + e + 1)))
            {
                let (mid, end) = end;
                put(&mut job, st, start, i, false);
                put(&mut job, st, i, i + 1, true);
                put(&mut job, Style { link: true, ..st }, i + 1, i + mid, false);
                put(&mut job, st, i + mid, i + end, true);
                i += end;
                start = i;
                continue;
            }
        }
        let Some(d) = delim else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let on = *flag(&mut st, d);
        let after = &rest[d.len()..];
        let opens = after.chars().next().is_some_and(|c| !c.is_whitespace()) && after.contains(d);
        if on || opens {
            put(&mut job, st, start, i, false);
            put(&mut job, st, i, i + d.len(), true); // the marker, in the style it opens or closes
            *flag(&mut st, d) = !on;
            start = i + d.len();
        }
        i += d.len();
    }
    put(&mut job, st, start, src.len(), false);
    if job.is_empty() {
        job.append("", 0.0, format(size, with_base(st)));
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (text, format) of every section.
    fn parts(job: &LayoutJob) -> Vec<(&str, &TextFormat)> {
        job.sections
            .iter()
            .map(|s| {
                (
                    &job.text[s.byte_range.start.0..s.byte_range.end.0],
                    &s.format,
                )
            })
            .collect()
    }

    #[test]
    fn styles_markdown_and_keeps_markers() {
        let src = "a **b** *c* `d` ~~e~~ [f](http://x) $g$";
        let dim = Color32::GRAY;
        let job = md_job(src, 16.0, false, dim);
        assert_eq!(job.text, src, "layouter must not change the text");
        let p = parts(&job);
        let get = |t: &str| p.iter().find(|(s, _)| *s == t).unwrap().1;
        assert_eq!(get("b").font_id.family, bold_family());
        assert!(get("c").italics);
        assert_eq!(get("d").font_id.family, FontFamily::Monospace);
        assert_ne!(get("e").strikethrough, Stroke::NONE);
        assert_eq!(get("f").color, LINK);
        assert!(get("g").italics);
        assert_eq!(get("**").color, dim, "markers stay visible, dimmed");
        assert_eq!(get("a ").font_id.family, FontFamily::Proportional);
    }

    #[test]
    fn mention_tokens_are_a_dimmed_frame_around_a_pill() {
        let src = "a **@[x\\]y](page:p1)** b";
        let dim = Color32::GRAY;
        let job = md_job(src, 16.0, false, dim);
        assert_eq!(job.text, src, "layouter must not change the text");
        let p = parts(&job);
        let get = |t: &str| p.iter().find(|(s, _)| *s == t).unwrap().1;
        assert_eq!(get("x\\]y").background, MENTION_BG);
        assert_eq!(get("x\\]y").font_id.family, bold_family(), "marks around it apply");
        assert_eq!(get("@[").color, dim);
        // egui merges equal neighbours: the closing `**` joins it
        assert_eq!(get("](page:p1)**").color, dim);
        assert_ne!(get("a ").background, MENTION_BG);
        // not a token: styled as the link it looks like
        let job = md_job("@[x] [y](u)", 16.0, false, dim);
        assert_eq!(job.text, "@[x] [y](u)");
        assert!(parts(&job).iter().all(|(_, f)| f.background != MENTION_BG));
    }

    #[test]
    fn unclosed_or_spaced_markers_stay_plain() {
        let job = md_job("2 * 3 and **open", 16.0, false, Color32::GRAY);
        assert_eq!(job.text, "2 * 3 and **open");
        assert!(parts(&job)
            .iter()
            .all(|(_, f)| !f.italics && f.font_id.family != bold_family()));
    }

    #[test]
    fn empty_and_multibyte_text_are_safe() {
        assert_eq!(md_job("", 16.0, false, Color32::GRAY).text, "");
        let s = "ß **ä** `ö` é";
        assert_eq!(md_job(s, 16.0, false, Color32::GRAY).text, s);
    }

    use serde_json::json;

    fn user_mention(plain: &str, id: &str) -> RichText {
        RichText {
            plain_text: plain.into(),
            href: None,
            annotations: Default::default(),
            kind: RichTextKind::Mention(json!({ "type": "user", "user": { "object": "user", "id": id } })),
        }
    }

    #[test]
    fn a_user_mention_shows_the_cached_name_with_an_at_sign() {
        let ada = User::from_value(&json!({ "object": "user", "id": "u1", "type": "person", "name": "Ada" })).unwrap();
        let text = |r: RichText, us: &[User]| rich_job(&[r], 16.0, false, us).text;
        assert_eq!(text(user_mention("@Old Name", "u1"), std::slice::from_ref(&ada)), "@Ada", "the cache is newer");
        // not cached: what the API (or our own token) said, with one "@"
        assert_eq!(text(user_mention("@Bob", "u2"), &[]), "@Bob");
        assert_eq!(text(user_mention("Bob", "u2"), std::slice::from_ref(&ada)), "@Bob");
        assert_eq!(text(user_mention("", "0123456789"), &[]), "@01234567");
        // other mentions are as they were
        let page = RichText {
            kind: RichTextKind::Mention(json!({ "type": "page", "page": { "id": "p" } })),
            ..user_mention("Home", "x")
        };
        assert_eq!(text(page, std::slice::from_ref(&ada)), "Home");
        let job = rich_job(&[user_mention("", "u1")], 16.0, false, &[]);
        assert!(job.sections[0].format.background != Color32::TRANSPARENT, "the mention pill");
    }
}
