//! LayoutJobs: read-only rich text, and live Markdown styling of the row being
//! edited (markers stay in the text, dimmed). Colours are `PLACEHOLDER`, so the
//! widget's own text colour (light or dark theme) applies.

use eframe::egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontFamily, FontId, Frame, Label, Margin, Stroke, Ui,
};
use notion_core::model::{RichText, RichTextKind, User};

use crate::tex::pretty_tex;
use crate::theme::Colors;

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
/// Semibold: Markdown bold and the headings.
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// The page title's heavier face (bold rather than semibold).
pub fn title_family() -> FontFamily {
    FontFamily::Name("title".into())
}

/// The serif face of equations (falls back to the normal face).
pub fn math_family() -> FontFamily {
    FontFamily::Name("math".into())
}

pub fn format(size: f32, s: Style) -> TextFormat {
    let (family, size) = if s.code {
        (FontFamily::Monospace, size * 0.9)
    } else if s.math {
        (math_family(), size * 1.06)
    } else if s.bold {
        (bold_family(), size)
    } else {
        (FontFamily::Proportional, size)
    };
    TextFormat {
        font_id: FontId::new(size, family),
        // Notion's reading rhythm: 1.5 for body text, tighter for headings.
        line_height: Some(if size >= 20.0 {
            size * 1.2
        } else if size >= 17.0 {
            size * 1.25
        } else {
            size * 1.5
        }),
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

/// Gives the runs that take the widget's text colour `c` (a block colour).
pub fn tint(job: &mut LayoutJob, c: Color32) {
    for s in &mut job.sections {
        if s.format.color == Color32::PLACEHOLDER {
            s.format.color = c;
        }
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
                put(
                    &mut job,
                    Style {
                        mention: true,
                        ..st
                    },
                    i + 2,
                    i + label,
                    false,
                );
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

/// Inline Markdown with the markers dropped: (text, style, link url).
fn inline(s: &str) -> Vec<(String, Style, Option<String>)> {
    let mut out = Vec::new();
    let mut st = Style::default();
    let mut buf = String::new();
    let mut i = 0;
    fn flush(out: &mut Vec<(String, Style, Option<String>)>, buf: &mut String, st: Style) {
        if !buf.is_empty() {
            out.push((std::mem::take(buf), st, None));
        }
    }
    while i < s.len() {
        let rest = &s[i..];
        let ch = rest.chars().next().unwrap_or(' ');
        if st.code {
            if ch == '`' {
                flush(&mut out, &mut buf, st);
                st.code = false;
            } else {
                buf.push(ch);
            }
            i += ch.len_utf8();
            continue;
        }
        let prev = s[..i].chars().next_back();
        let d = ["**", "~~", "`", "*", "_"].into_iter().find(|d| rest.starts_with(d));
        if let Some(d) = d {
            let n = d.len();
            let on = match d {
                "**" => st.bold,
                "~~" => st.strike,
                "`" => false,
                _ => st.italic,
            };
            let ok = on
                || (rest[n..].contains(d)
                    && !rest[n..].starts_with(char::is_whitespace)
                    && !(d == "_" && prev.is_some_and(char::is_alphanumeric)));
            if ok {
                flush(&mut out, &mut buf, st);
                match d {
                    "**" => st.bold = !on,
                    "~~" => st.strike = !on,
                    "`" => st.code = true,
                    _ => st.italic = !on,
                }
                i += n;
                continue;
            }
        } else if ch == '[' {
            if let Some((label, url)) = rest.find("](").and_then(|m| {
                let e = rest[m..].find(')')?;
                Some((&rest[1..m], &rest[m + 2..m + e]))
            }) {
                flush(&mut out, &mut buf, st);
                out.push((label.to_string(), st, Some(url.to_string())));
                i += label.len() + url.len() + 4;
                continue;
            }
        }
        buf.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut out, &mut buf, st);
    out
}

#[derive(Debug, PartialEq)]
enum Block {
    /// heading level 1..=6
    Head(usize, String),
    /// nesting level, marker, text
    Item(usize, String, String),
    Code(String),
    Quote(String),
    Rule,
    Para(String),
    Table(String),
}

/// Lines to blocks; an unclosed fence (a streaming answer) runs to the end.
fn blocks(text: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut fence: Option<String> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            match fence.take() {
                Some(code) => out.push(Block::Code(code)),
                None => fence = Some(String::new()),
            }
            continue;
        }
        if let Some(code) = &mut fence {
            if !code.is_empty() {
                code.push('\n');
            }
            code.push_str(line);
            continue;
        }
        let level = (line.len() - t.len()) / 2;
        let hashes = t.chars().take_while(|&c| c == '#').count();
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        let t = t.trim_end();
        if t.is_empty() {
            continue;
        } else if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            out.push(Block::Head(hashes, t[hashes..].trim().to_string()));
        } else if t.len() >= 3 && t.chars().all(|c| c == '-') | t.chars().all(|c| c == '*') | t.chars().all(|c| c == '_') {
            out.push(Block::Rule);
        } else if let Some(q) = t.strip_prefix('>') {
            out.push(Block::Quote(q.trim_start().to_string()));
        } else if let Some(r) = ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m)) {
            out.push(Block::Item(level, "•".into(), r.to_string()));
        } else if digits > 0 && t[digits..].starts_with(". ") {
            out.push(Block::Item(level, t[..=digits].to_string(), t[digits + 2..].to_string()));
        } else if t.starts_with('|') {
            match out.last_mut() {
                Some(Block::Table(s)) => {
                    s.push('\n');
                    s.push_str(t);
                }
                _ => out.push(Block::Table(t.to_string())),
            }
        } else {
            out.push(Block::Para(t.to_string()));
        }
    }
    if let Some(code) = fence {
        out.push(Block::Code(code));
    }
    out
}

/// A line of inline Markdown, wrapped, selectable; links are clickable.
fn show_inline(ui: &mut Ui, c: &Colors, text: &str, size: f32, bold: bool) {
    let spans = inline(text);
    let fmt = |st: Style| {
        let mut f = format(size, Style { bold: st.bold || bold, ..st });
        if f.color == Color32::PLACEHOLDER {
            f.color = c.text;
        }
        f
    };
    if spans.iter().all(|s| s.2.is_none()) {
        let mut job = LayoutJob::default();
        for (t, st, _) in &spans {
            job.append(t, 0.0, fmt(*st));
        }
        ui.add(Label::new(job).selectable(true));
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (t, st, url) in &spans {
            // only web links: AI output must not open file: or app handlers
            let web = |u: &&String| u.starts_with("https://") || u.starts_with("http://");
            match url.as_ref().filter(web) {
                Some(u) => {
                    ui.hyperlink_to(eframe::egui::RichText::new(t).size(size).color(c.accent), u);
                }
                None => {
                    let mut job = LayoutJob::default();
                    job.append(t, 0.0, fmt(*st));
                    ui.add(Label::new(job).selectable(true));
                }
            }
        }
    });
}

/// Markdown rendered with the markers hidden (the AI panel's answer).
pub(crate) fn show_markdown(ui: &mut Ui, c: &Colors, text: &str) {
    for b in blocks(text) {
        match b {
            Block::Head(n, t) => {
                ui.add_space(4.0);
                show_inline(ui, c, &t, [22.0, 19.0, 16.5, 15.0, 14.5, 14.0][n - 1], true);
            }
            Block::Item(level, m, t) => {
                let left = (14 * level.min(8) + 4) as i8;
                Frame::new()
                    .inner_margin(eframe::egui::Margin { left, ..Default::default() })
                    .show(ui, |ui| show_inline(ui, c, &format!("{m} {t}"), 14.0, false));
            }
            Block::Code(s) | Block::Table(s) => {
                Frame::new()
                    .fill(c.hover)
                    .corner_radius(6)
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.add(
                            Label::new(eframe::egui::RichText::new(s).monospace().size(13.0).color(c.text))
                                .selectable(true),
                        );
                    });
            }
            Block::Quote(t) => {
                let r = Frame::new()
                    .inner_margin(Margin { left: 12, ..Default::default() })
                    .show(ui, |ui| show_inline(ui, c, &t, 14.0, false));
                let b = r.response.rect;
                ui.painter().vline(b.left() + 2.0, b.y_range(), Stroke::new(3.0, c.border));
            }
            Block::Rule => {
                ui.separator();
            }
            Block::Para(t) => show_inline(ui, c, &t, 14.0, false),
        }
        ui.add_space(3.0);
    }
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
        assert_eq!(
            get("x\\]y").font_id.family,
            bold_family(),
            "marks around it apply"
        );
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
            kind: RichTextKind::Mention(
                json!({ "type": "user", "user": { "object": "user", "id": id } }),
            ),
        }
    }

    #[test]
    fn a_user_mention_shows_the_cached_name_with_an_at_sign() {
        let ada = User::from_value(
            &json!({ "object": "user", "id": "u1", "type": "person", "name": "Ada" }),
        )
        .unwrap();
        let text = |r: RichText, us: &[User]| rich_job(&[r], 16.0, false, us).text;
        assert_eq!(
            text(user_mention("@Old Name", "u1"), std::slice::from_ref(&ada)),
            "@Ada",
            "the cache is newer"
        );
        // not cached: what the API (or our own token) said, with one "@"
        assert_eq!(text(user_mention("@Bob", "u2"), &[]), "@Bob");
        assert_eq!(
            text(user_mention("Bob", "u2"), std::slice::from_ref(&ada)),
            "@Bob"
        );
        assert_eq!(text(user_mention("", "0123456789"), &[]), "@01234567");
        // other mentions are as they were
        let page = RichText {
            kind: RichTextKind::Mention(json!({ "type": "page", "page": { "id": "p" } })),
            ..user_mention("Home", "x")
        };
        assert_eq!(text(page, std::slice::from_ref(&ada)), "Home");
        let job = rich_job(&[user_mention("", "u1")], 16.0, false, &[]);
        assert!(
            job.sections[0].format.background != Color32::TRANSPARENT,
            "the mention pill"
        );
    }

    #[test]
    fn markdown_inline_and_blocks() {
        let v = inline("**a** `b` [x](http://u) _i_");
        assert_eq!((v[0].0.as_str(), v[0].1.bold), ("a", true));
        assert_eq!((v[2].0.as_str(), v[2].1.code), ("b", true));
        assert_eq!(v[4].2.as_deref(), Some("http://u"));
        assert!(v[6].1.italic);
        let b = blocks("# T\n- x\n  1. y\n```rs\nlet a;\n");
        assert_eq!(b[0], Block::Head(1, "T".into()));
        assert_eq!(b[2], Block::Item(1, "1.".into(), "y".into()));
        assert_eq!(b[3], Block::Code("let a;".into()));
        inline("**unclosed `x [a](");
    }

    fn flat(s: &str) -> String {
        inline(s).into_iter().map(|v| v.0).collect()
    }

    #[test]
    fn markdown_edge_cases() {
        let v = inline("***x***");
        assert_eq!(flat("***x***"), "x");
        assert!(v.iter().any(|s| s.1.bold && s.1.italic), "{v:?}");
        let v = inline("snake_case_word");
        assert_eq!(flat("snake_case_word"), "snake_case_word");
        assert!(v.iter().all(|s| !s.1.italic));
        // link without closing paren stays literal
        assert_eq!(flat("[a](http://x"), "[a](http://x");
        assert!(inline("[a](http://x").iter().all(|s| s.2.is_none()));
        assert_eq!(flat("[a] (b)"), "[a] (b)");
        assert!(inline("").is_empty());
        assert!(blocks("").is_empty());
        assert!(blocks("\n  \n").is_empty());
        assert_eq!(blocks("10. ten")[0], Block::Item(0, "10.".into(), "ten".into()));
        assert_eq!(blocks("10.5 not a list")[0], Block::Para("10.5 not a list".into()));
        assert_eq!(blocks("---")[0], Block::Rule);
        assert_eq!(blocks("> q")[0], Block::Quote("q".into()));
        assert_eq!(blocks("#nospace")[0], Block::Para("#nospace".into()));
        assert_eq!(blocks("####### x")[0], Block::Para("####### x".into()));
        let t = blocks("| a | b |\n|---|---|\n| 1 | 2 |");
        assert_eq!(t.len(), 1);
        assert!(matches!(&t[0], Block::Table(s) if s.lines().count() == 3));
        // multibyte next to markers and links
        assert_eq!(flat("ä**ö**[é](u)ü"), "äöéü");
    }

    #[test]
    fn show_markdown_survives_a_long_mixed_doc() {
        let doc = "# H1\n###### H6\ntext **b** *i* `c` ~~s~~ [l](http://x) [bad](x\n- a\n  - b\n    - c\n1. one\n10. ten\n> quote\n---\n| a | b |\n|---|---|\n| 1 | 2 |\n```rs\nfn main() {}\n```\nüñí **ä**\n```\nunclosed";
        let ctx = eframe::egui::Context::default();
        let c = crate::theme::LIGHT;
        for text in [doc.to_string(), doc.repeat(30), String::new()] {
            let raw = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(400.0, 800.0),
                )),
                ..Default::default()
            };
            ctx.run_ui(raw, |ui| show_markdown(ui, &c, &text)).drop_without_applying_deltas();
        }
    }
}
