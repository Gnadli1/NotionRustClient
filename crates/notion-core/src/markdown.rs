//! Rich text <-> a small Markdown subset: `**bold**`, `*italic*`, `` `code` ``,
//! `~~strike~~`, `[text](url)`, `$equation$`, and mentions as
//! `@[label](page:<id>)`, `@[label](database:<id>)`, `@[label](user:<id>)`,
//! `@[label](date:<start>[/<end>])`.
//!
//! Limitations: underline and colour are dropped; other mention types
//! (link_preview, template_mention, custom emoji, a date with a time zone)
//! can't be written, so such text is read-only, see
//! [`is_markdown_editable`]. A mention's `href` is dropped (Notion derives it)
//! and its `plain_text` is the label. Inside code spans only `\` and `` ` `` are
//! escaped; in link URLs `(` and `)` are escaped too. An equation's TeX is
//! written raw: `$` must be followed by a non-space and the closing `$`
//! preceded by one, and `\$` inside it doesn't close it. Equations keep
//! bold/italic/strike; link and code marks are dropped.

use serde_json::{json, Value};

use crate::model::{Annotations, RichText, RichTextKind, TargetKind};

/// Notion's per-run character limit.
const MAX_RUN: usize = 2000;
/// Characters backslash-escaped in plain text.
const ESCAPED: &[char] = &['\\', '*', '`', '~', '[', ']', '$'];
const CODE_ESCAPED: &[char] = &['\\', '`'];
const URL_ESCAPED: &[char] = &['\\', '*', '`', '~', '[', ']', '(', ')'];

#[derive(Debug, Clone, PartialEq, Default)]
struct Style {
    link: Option<String>,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    /// An inline equation; its text is the TeX expression. Never merged.
    eq: bool,
    /// A mention's `kind:target` (see [`mention_target`]); its text is the
    /// label. Never merged.
    mention: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Mark {
    Link(String),
    Bold,
    Italic,
    Strike,
    Code,
}

impl Style {
    /// Markers to have open, outer → inner.
    fn marks(&self) -> Vec<Mark> {
        let mut m = Vec::new();
        if let Some(u) = &self.link {
            m.push(Mark::Link(u.clone()));
        }
        for (on, mark) in [
            (self.bold, Mark::Bold),
            (self.italic, Mark::Italic),
            (self.strike, Mark::Strike),
            (self.code, Mark::Code),
        ] {
            if on {
                m.push(mark);
            }
        }
        m
    }
}

/// (text, style) per run, underline/colour dropped, adjacent equal styles merged.
fn normalize(rt: &[RichText]) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    for r in rt {
        let (text, link, eq, mention) = match &r.kind {
            RichTextKind::Text { content, link } => (content.clone(), link.clone(), false, None),
            RichTextKind::Equation { expression } => (expression.clone(), None, true, None),
            RichTextKind::Mention(v) => match mention_target(v) {
                Some(t) => (r.plain_text.clone(), None, false, Some(t)),
                None => (r.plain_text.clone(), r.href.clone(), false, None),
            },
        };
        if text.is_empty() && mention.is_none() {
            continue;
        }
        let a = &r.annotations;
        let style = Style {
            link,
            bold: a.bold,
            italic: a.italic,
            strike: a.strikethrough,
            code: a.code && !eq,
            eq,
            mention,
        };
        match out.last_mut() {
            Some((t, s)) if *s == style && !style.eq && style.mention.is_none() => {
                t.push_str(&text)
            }
            _ => out.push((text, style)),
        }
    }
    out
}

fn open_str(m: &Mark) -> &'static str {
    match m {
        Mark::Link(_) => "[",
        Mark::Bold => "**",
        Mark::Italic => "*",
        Mark::Strike => "~~",
        Mark::Code => "`",
    }
}

fn push_escaped(out: &mut String, s: &str, set: &[char]) {
    for c in s.chars() {
        if set.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
}

fn close(out: &mut String, m: &Mark) {
    match m {
        Mark::Link(u) => {
            out.push_str("](");
            push_escaped(out, u, URL_ESCAPED);
            out.push(')');
        }
        m => out.push_str(open_str(m)),
    }
}

pub fn to_markdown(rt: &[RichText]) -> String {
    let mut out = String::new();
    let mut open: Vec<Mark> = Vec::new();
    for (text, style) in normalize(rt) {
        let want = style.marks();
        let keep = open.iter().zip(&want).take_while(|(a, b)| a == b).count();
        while open.len() > keep {
            close(&mut out, &open.pop().unwrap());
        }
        for m in &want[keep..] {
            out.push_str(open_str(m));
            open.push(m.clone());
        }
        if let Some(t) = &style.mention {
            out.push_str("@[");
            push_escaped(&mut out, &text, ESCAPED);
            out.push_str("](");
            push_escaped(&mut out, t, URL_ESCAPED);
            out.push(')');
        } else if style.eq {
            out.push('$');
            out.push_str(&text);
            out.push('$');
        } else {
            push_escaped(
                &mut out,
                &text,
                if style.code { CODE_ESCAPED } else { ESCAPED },
            );
        }
    }
    while let Some(m) = open.pop() {
        close(&mut out, &m);
    }
    out
}

fn id_of<'a>(v: &'a Value, kind: &str) -> Option<&'a str> {
    let id = v.get(kind)?.get("id")?.as_str()?;
    (!id.is_empty() && !id.chars().any(char::is_whitespace)).then_some(id)
}

/// The `kind:target` of a mention the token can express: page, database and
/// user by id, date by `start[/end]`. None for everything else (and for a
/// date with a time zone, which the token has no room for).
fn mention_target(v: &Value) -> Option<String> {
    let t = raw_target(v)?;
    // only targets that parse back to themselves
    let back = mention_from_target(&t)?;
    (raw_target(&back).as_deref() == Some(&t)).then_some(t)
}

fn raw_target(v: &Value) -> Option<String> {
    Some(match v.get("type")?.as_str()? {
        k @ ("page" | "database" | "user") => format!("{k}:{}", id_of(v, k)?),
        "date" => {
            let d = v.get("date")?;
            if d.get("time_zone").is_some_and(|z| !z.is_null()) {
                return None;
            }
            let start = d.get("start")?.as_str()?;
            match d.get("end").and_then(Value::as_str) {
                Some(end) => format!("date:{start}/{end}"),
                None => format!("date:{start}"),
            }
        }
        _ => return None,
    })
}

/// The local date `offset_days` from today, as `YYYY-MM-DD` (for date mentions).
pub fn local_date_iso(offset_days: i64) -> String {
    (chrono::Local::now().date_naive() + chrono::TimeDelta::days(offset_days))
        .format("%Y-%m-%d")
        .to_string()
}

/// The API's mention JSON for a `kind:target`.
fn mention_from_target(t: &str) -> Option<Value> {
    let (kind, rest) = t.split_once(':')?;
    if rest.is_empty() || rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(match kind {
        "page" | "database" => json!({ "type": kind, kind: { "id": rest } }),
        "user" => json!({ "type": "user", "user": { "object": "user", "id": rest } }),
        "date" => {
            let (start, end) = match rest.split_once('/') {
                Some((s, e)) if !s.is_empty() && !e.is_empty() => (s, Some(e)),
                Some(_) => return None,
                None => (rest, None),
            };
            // a real calendar date (`YYYY-MM-DD`) or an RFC 3339 datetime
            let dated = |s: &str| {
                (s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok())
                    || chrono::DateTime::parse_from_rfc3339(s).is_ok()
            };
            if !dated(start) || end.is_some_and(|e| !dated(e)) {
                return None;
            }
            json!({ "type": "date", "date": { "start": start, "end": end } })
        }
        _ => return None,
    })
}

/// `@[label](kind:target)` at `cs[i]` (the `@`): (label, target, index past `)`).
fn scan_mention(cs: &[char], i: usize) -> Option<(String, String, usize)> {
    if cs.get(i + 1) != Some(&'[') {
        return None;
    }
    let (mut label, mut j) = (String::new(), i + 2);
    loop {
        match *cs.get(j)? {
            '\\' if cs.get(j + 1).is_some_and(|n| ESCAPED.contains(n)) => {
                label.push(cs[j + 1]);
                j += 2;
            }
            ']' => break,
            c => {
                label.push(c);
                j += 1;
            }
        }
    }
    if cs.get(j + 1) != Some(&'(') {
        return None;
    }
    let (target, end) = scan(cs, j + 2, ')', URL_ESCAPED)?;
    mention_from_target(&target)?;
    Some((label, target, end))
}

enum Item {
    Ch(char),
    Mention(String, String),
    Lit(&'static str),
    Code(String),
    Eq(String),
    Open(Mark),
    Close,
}

/// Closes the innermost open mark matching `want`; openers above it become
/// literal text. Returns the closed opener's item index.
///
/// An empty span (closer right after its opener) is literal instead: the
/// opener becomes text, the `closer` text is pushed if given and its index
/// returned, else None (the caller emits the closer as text).
fn close_mark(
    items: &mut Vec<Item>,
    stack: &mut Vec<(Mark, usize)>,
    want: impl Fn(&Mark) -> bool,
    closer: Option<&'static str>,
) -> Option<usize> {
    let pos = stack.iter().rposition(|(m, _)| want(m))?;
    for (m, at) in stack.drain(pos + 1..) {
        items[at] = Item::Lit(open_str(&m));
    }
    let (m, at) = stack.pop().unwrap();
    if at + 1 == items.len() {
        items[at] = Item::Lit(open_str(&m));
        items.push(Item::Lit(closer?));
    } else {
        items.push(Item::Close);
    }
    Some(at)
}

/// Text from `i` up to the first unescaped `end` (a backslash escapes chars in
/// `esc`); returns (text, index past `end`), or None if unterminated or empty.
fn scan(cs: &[char], mut i: usize, end: char, esc: &[char]) -> Option<(String, usize)> {
    let mut s = String::new();
    while i < cs.len() {
        match cs[i] {
            '\\' if cs.get(i + 1).is_some_and(|n| esc.contains(n)) => {
                s.push(cs[i + 1]);
                i += 2;
            }
            c if c == end => return (!s.is_empty()).then_some((s, i + 1)),
            c => {
                s.push(c);
                i += 1;
            }
        }
    }
    None
}

/// An equation's TeX from `i` (just past the opening `$`) to its closing `$`;
/// returns (tex, index past `$`). `\x` is kept as is and never closes.
fn scan_eq(cs: &[char], mut i: usize) -> Option<(String, usize)> {
    let start = i;
    if cs.get(i).is_none_or(|c| c.is_whitespace()) {
        return None;
    }
    while i < cs.len() {
        match cs[i] {
            '\\' => i += 2,
            // Pandoc: not `$` + space, and a digit after it means a price
            '$' if i > start
                && !cs[i - 1].is_whitespace()
                && !cs.get(i + 1).is_some_and(char::is_ascii_digit) =>
            {
                return Some((cs[start..i].iter().collect(), i + 1));
            }
            _ => i += 1,
        }
    }
    None
}

pub fn from_markdown(s: &str) -> Vec<RichText> {
    let cs: Vec<char> = s.chars().collect();
    let mut items: Vec<Item> = Vec::new();
    // (mark, index of its Open item)
    let mut stack: Vec<(Mark, usize)> = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        let mark = match (c, next) {
            ('\\', Some(n)) if ESCAPED.contains(&n) => {
                items.push(Item::Ch(n));
                i += 2;
                continue;
            }
            ('`', _) => {
                if let Some((code, end)) = scan(&cs, i + 1, '`', CODE_ESCAPED) {
                    items.push(Item::Code(code));
                    i = end;
                    continue;
                }
                None
            }
            ('@', _) => {
                if let Some((label, target, end)) = scan_mention(&cs, i) {
                    items.push(Item::Mention(label, target));
                    i = end;
                    continue;
                }
                None
            }
            ('$', _) => {
                if let Some((tex, end)) = scan_eq(&cs, i + 1) {
                    items.push(Item::Eq(tex));
                    i = end;
                    continue;
                }
                None
            }
            ('*', _) if matches!(stack.last(), Some((Mark::Italic, _))) => Some(Mark::Italic),
            ('*', Some('*')) => Some(Mark::Bold),
            ('*', _) => Some(Mark::Italic),
            ('~', Some('~')) => Some(Mark::Strike),
            ('[', _) => Some(Mark::Link(String::new())),
            (']', Some('(')) => {
                let url = scan(&cs, i + 2, ')', URL_ESCAPED);
                if let Some((url, end)) = url {
                    if let Some(at) =
                        close_mark(&mut items, &mut stack, |m| matches!(m, Mark::Link(_)), None)
                    {
                        items[at] = Item::Open(Mark::Link(url));
                        i = end;
                        continue;
                    }
                }
                None
            }
            _ => None,
        };
        match mark {
            None => {
                items.push(Item::Ch(c));
                i += 1;
            }
            Some(m) => {
                i += open_str(&m).len();
                let is_link = matches!(m, Mark::Link(_));
                if is_link
                    || close_mark(&mut items, &mut stack, |x| *x == m, Some(open_str(&m))).is_none()
                {
                    stack.push((m.clone(), items.len()));
                    items.push(Item::Open(m));
                }
            }
        }
    }
    // Unmatched openers are literal text.
    for (m, at) in stack {
        items[at] = Item::Lit(open_str(&m));
    }

    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut active: Vec<Mark> = Vec::new();
    let mut push = |text: &str, code: bool, eq: bool, mention: Option<String>, active: &[Mark]| {
        let atom = eq || mention.is_some();
        let mut st = Style {
            code,
            eq,
            mention,
            ..Style::default()
        };
        for m in active {
            match m {
                Mark::Link(_) if atom => {}
                Mark::Link(u) => st.link = Some(u.clone()),
                Mark::Bold => st.bold = true,
                Mark::Italic => st.italic = true,
                Mark::Strike => st.strike = true,
                Mark::Code => st.code = true,
            }
        }
        match runs.last_mut() {
            Some((t, s)) if *s == st && !atom => t.push_str(text),
            _ => runs.push((text.to_string(), st)),
        }
    };
    for it in items {
        match it {
            Item::Ch(c) => push(c.encode_utf8(&mut [0; 4]), false, false, None, &active),
            Item::Lit(s) => push(s, false, false, None, &active),
            Item::Code(t) => push(&t, true, false, None, &active),
            Item::Eq(t) => push(&t, false, true, None, &active),
            Item::Mention(l, t) => push(&l, false, false, Some(t), &active),
            Item::Open(m) => active.push(m),
            Item::Close => {
                active.pop();
            }
        }
    }
    split_runs(
        runs.into_iter()
            .map(|(t, st)| RichText {
                plain_text: t.clone(),
                href: st.link.clone(),
                annotations: Annotations {
                    bold: st.bold,
                    italic: st.italic,
                    strikethrough: st.strike,
                    code: st.code,
                    ..Annotations::default()
                },
                kind: if st.eq {
                    RichTextKind::Equation { expression: t }
                } else if let Some(m) = st.mention.as_deref().and_then(mention_from_target) {
                    RichTextKind::Mention(m)
                } else {
                    RichTextKind::Text {
                        content: t,
                        link: st.link,
                    }
                },
            })
            .collect(),
    )
}

/// True if editing `rt` as Markdown loses nothing beyond underline/colour:
/// every mention is one the `@[label](kind:id)` token carries, and it survives
/// `to_markdown` → `from_markdown` (an equation with a bare `$`, edge spaces,
/// or a digit right after it doesn't).
pub fn is_markdown_editable(rt: &[RichText]) -> bool {
    rt.iter()
        .all(|r| !matches!(&r.kind, RichTextKind::Mention(v) if mention_target(v).is_none()))
        && normalize(&from_markdown(&to_markdown(rt))) == normalize(rt)
}

/// Splits text runs longer than Notion's 2000-char limit, keeping their style.
pub(crate) fn split_runs(rt: Vec<RichText>) -> Vec<RichText> {
    let mut out = Vec::new();
    for r in rt {
        let content = match &r.kind {
            RichTextKind::Text { content, .. } if content.chars().count() > MAX_RUN => {
                content.clone()
            }
            _ => {
                out.push(r);
                continue;
            }
        };
        let chars: Vec<char> = content.chars().collect();
        for chunk in chars.chunks(MAX_RUN) {
            let t: String = chunk.iter().collect();
            let mut piece = r.clone();
            piece.plain_text = t.clone();
            if let RichTextKind::Text { content, .. } = &mut piece.kind {
                *content = t;
            }
            out.push(piece);
        }
    }
    out
}

/// A block-type shortcut typed at the very start of a block (`# `, `- `,
/// `1. `, `[] `, `> `, `` ``` ``, `---`, ...) and the text after it. For
/// `` ``` `` the rest is returned as text, not parsed as a language.
pub fn markdown_shortcut(text: &str) -> Option<(TargetKind, &str)> {
    use TargetKind::*;
    if text == "---" {
        return Some((Divider, ""));
    }
    if let Some(rest) = text.strip_prefix("```") {
        return Some((Code, rest));
    }
    let prefixes = [
        ("# ", Heading1),
        ("## ", Heading2),
        ("### ", Heading3),
        ("- ", Bulleted),
        ("* ", Bulleted),
        ("[] ", ToDo),
        ("[ ] ", ToDo),
        ("> ", Quote),
        ("$$ ", Equation),
    ];
    for (p, k) in prefixes {
        if let Some(rest) = text.strip_prefix(p) {
            return Some((k, rest));
        }
    }
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (digits > 0)
        .then(|| text[digits..].strip_prefix(". "))
        .flatten()
        .map(|rest| (Numbered, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(text: &str, link: Option<&str>, b: bool, i: bool, s: bool, c: bool) -> RichText {
        let mut r = RichText::plain(text);
        r.annotations = Annotations {
            bold: b,
            italic: i,
            strikethrough: s,
            code: c,
            ..Annotations::default()
        };
        if let Some(u) = link {
            r.href = Some(u.into());
            r.kind = RichTextKind::Text {
                content: text.into(),
                link: Some(u.into()),
            };
        }
        r
    }
    fn p(t: &str) -> RichText {
        rt(t, None, false, false, false, false)
    }
    fn b(t: &str) -> RichText {
        rt(t, None, true, false, false, false)
    }
    fn i(t: &str) -> RichText {
        rt(t, None, false, true, false, false)
    }
    fn bi(t: &str) -> RichText {
        rt(t, None, true, true, false, false)
    }
    fn code(t: &str) -> RichText {
        rt(t, None, false, false, false, true)
    }

    /// Canonical (merged, default colour) rich text survives exactly.
    #[test]
    fn round_trips_exactly() {
        let cases: Vec<Vec<RichText>> = vec![
            vec![p("plain text")],
            vec![p("a "), b("bold"), p(" b")],
            vec![bi("bi")],
            vec![p("x "), b("bold"), bi("both"), b("bold")],
            vec![
                rt(
                    "go ",
                    Some("http://e.com/a_(b)"),
                    false,
                    false,
                    false,
                    false,
                ),
                rt(
                    "here",
                    Some("http://e.com/a_(b)"),
                    true,
                    false,
                    false,
                    false,
                ),
            ],
            vec![code("a*b"), p(" x")],
            vec![code(r"a`b\c``"), p(" x")],
            vec![p(r"lit * [x] \ ` ~ ~~ ** end \n C:\path (y)")],
            vec![b("b"), i("i")],
            vec![i("i"), b("b")],
            vec![i("i"), bi("bi")],
            vec![bi("bi"), i("i")],
            vec![bi("bi"), b("b")],
            vec![b("b"), bi("bi")],
            vec![rt("lb", Some("u"), true, false, false, false), b("b")],
            vec![
                rt("a", Some("u1"), false, false, false, false),
                rt("b", Some("u2"), false, false, false, false),
            ],
            vec![
                rt("s", None, false, false, true, false),
                rt("si", None, false, true, true, false),
                rt("c", None, true, false, true, true),
            ],
            vec![rt("all", Some("u"), true, true, true, true), p("\nline 2")],
            vec![rt("bc", None, true, false, false, true), code("c")],
        ];
        for c in cases {
            let md = to_markdown(&c);
            assert_eq!(from_markdown(&md), c, "markdown: {md}");
        }
    }

    #[test]
    fn drops_underline_colour_and_merges() {
        let mut u = b("u");
        u.annotations.underline = true;
        u.annotations.color = "red".into();
        let md = to_markdown(&[p("a"), p("b"), b("x"), u]);
        assert_eq!(md, "ab**xu**");
        assert_eq!(from_markdown(&md), vec![p("ab"), b("xu")]);
    }

    #[test]
    fn parses_exact_runs() {
        assert_eq!(
            from_markdown("a **b *c* d** e"),
            vec![p("a "), b("b "), bi("c"), b(" d"), p(" e")]
        );
        assert_eq!(
            from_markdown("[x](http://y)"),
            vec![rt("x", Some("http://y"), false, false, false, false)]
        );
        assert_eq!(from_markdown("`a*b`"), vec![code("a*b")]);
        assert_eq!(from_markdown("**unclosed"), vec![p("**unclosed")]);
        assert_eq!(from_markdown(r"\*x\* [y"), vec![p("*x* [y")]);
        assert_eq!(
            from_markdown("~~s~~ `open"),
            vec![rt("s", None, false, false, true, false), p(" `open")]
        );
        // unmatched openers stay literal, in place
        assert_eq!(from_markdown("~~a *b"), vec![p("~~a *b")]);
        // an unmatched inner opener doesn't block the outer close
        assert_eq!(from_markdown("**a [b] c**"), vec![b("a [b] c")]);
        // `*` closes an open italic first: `**` here is close + reopen
        assert_eq!(from_markdown("*a **b* c"), vec![i("a b"), p(" c")]);
        // empty link URL / empty code span are literal
        assert_eq!(from_markdown("[x]()"), vec![p("[x]()")]);
        assert_eq!(from_markdown("a``b"), vec![p("a``b")]);
        // empty spans don't swallow what was typed
        for lit in ["****", "~~~~", "[](u)", "a ** b", "******"] {
            assert_eq!(from_markdown(lit), vec![p(lit)], "{lit}");
        }
        assert_eq!(from_markdown("**a****b**"), vec![b("ab")]); // close + reopen
                                                                // backslash only escapes what to_markdown escapes
        assert_eq!(from_markdown(r"C:\path \( \\"), vec![p(r"C:\path \( \")]);
        assert_eq!(from_markdown("single ~ tilde"), vec![p("single ~ tilde")]);
    }

    #[test]
    fn markdown_is_stable() {
        let nasty = [
            "**a *b** c*",
            "***x***y*",
            "**a*b**",
            "~~a **b~~ c**",
            "[a **b](u) c**",
            "[a [b](u)](v)",
            "[x](u",
            "`a\\`b` \\",
            "*a* **b** ~~c~~ `d` [e](f)",
            "****",
            "*",
            "~~~",
            "]][[",
            "\\",
            "a\nb **c\nd**",
            "$a$$b$ $ $$ $c\\$ d$",
            "**$x** $y$",
            "@[a](page:1) @[b",
            "[@[a](page:1)](u) @[](",
            "`@[a](page:1)` @[a\\]b](user:2)",
        ];
        for s in nasty {
            let m = to_markdown(&from_markdown(s));
            assert_eq!(to_markdown(&from_markdown(&m)), m, "input: {s}");
        }
    }

    #[test]
    fn long_runs_split() {
        let long = from_markdown(&"x".repeat(4500));
        let lens: Vec<usize> = long.iter().map(|r| r.plain_text.len()).collect();
        assert_eq!(lens, [2000, 2000, 500]);
        let long = from_markdown(&format!("**{}**", "é".repeat(2001)));
        assert_eq!(long.len(), 2);
        assert!(long.iter().all(|r| r.annotations.bold));
    }

    fn mention(label: &str, m: serde_json::Value) -> RichText {
        RichText {
            plain_text: label.into(),
            href: None,
            annotations: Annotations::default(),
            kind: RichTextKind::Mention(m),
        }
    }

    fn page(label: &str, id: &str) -> RichText {
        mention(
            label,
            serde_json::json!({ "type": "page", "page": { "id": id } }),
        )
    }

    #[test]
    fn mentions_are_editable_only_when_the_token_carries_them() {
        use serde_json::json;
        assert!(is_markdown_editable(&[p("x")]));
        assert!(is_markdown_editable(&[p("x"), eq("x^2")]));
        assert!(is_markdown_editable(&[p("x"), page("P", "abc")]));
        let date = json!({ "type": "date", "date": { "start": "2026-11-20", "end": null, "time_zone": null } });
        assert!(is_markdown_editable(&[mention("2026-11-20", date)]));
        // as the API returns a user mention (extra fields are metadata)
        let user =
            json!({ "type": "user", "user": { "object": "user", "id": "u1", "name": "Bob" } });
        assert!(is_markdown_editable(&[mention("@Bob", user)]));
        // what the token can't carry stays read-only
        for bad in [
            json!({ "type": "user" }),
            json!({ "type": "link_preview", "link_preview": { "url": "https://x" } }),
            json!({ "type": "template_mention", "template_mention": { "type": "template_mention_date", "template_mention_date": "today" } }),
            json!({ "type": "custom_emoji", "custom_emoji": { "id": "e", "name": "n" } }),
            json!({ "type": "link_mention", "link_mention": { "href": "https://x" } }),
            json!({ "type": "page", "page": {} }),
            json!({ "type": "date", "date": { "start": "2026-11-20T10:00:00", "time_zone": "Europe/Vienna" } }),
        ] {
            assert!(
                !is_markdown_editable(&[p("x"), mention("m", bad.clone())]),
                "{bad}"
            );
        }
        // TeX that `$…$` can't carry stays read-only
        for bad in ["a$", " x", "x ", "x\\"] {
            assert!(!is_markdown_editable(&[eq(bad)]), "{bad:?}");
        }
    }

    #[test]
    fn mentions_round_trip() {
        use serde_json::json;
        let range =
            json!({ "type": "date", "date": { "start": "2026-11-20", "end": "2026-11-22" } });
        let day = json!({ "type": "date", "date": { "start": "2026-11-20", "end": null } });
        let mut bold = page("B", "id9");
        bold.annotations.bold = true;
        let mut it = page("I", "id8");
        it.annotations.italic = true;
        let cases: Vec<(Vec<RichText>, &str)> = vec![
            (vec![page("Home", "p-1")], "@[Home](page:p-1)"),
            (
                vec![mention(
                    "DB",
                    json!({ "type": "database", "database": { "id": "d1" } }),
                )],
                "@[DB](database:d1)",
            ),
            (
                vec![mention(
                    "@Bob",
                    json!({ "type": "user", "user": { "object": "user", "id": "u1" } }),
                )],
                "@[@Bob](user:u1)",
            ),
            (vec![mention("20 Nov", day)], "@[20 Nov](date:2026-11-20)"),
            (
                vec![mention("range", range)],
                "@[range](date:2026-11-20/2026-11-22)",
            ),
            (
                vec![p("see "), bold, p(" and "), it],
                "see **@[B](page:id9)** and *@[I](page:id8)*",
            ),
            // `]`, `\`, `*` and `[` in the label are escaped
            (
                vec![page("a]b [c] *d* \\e", "x")],
                "@[a\\]b \\[c\\] \\*d\\* \\\\e](page:x)",
            ),
            (vec![page("", "empty")], "@[](page:empty)"),
            (
                vec![page("a", "1"), page("b", "2")],
                "@[a](page:1)@[b](page:2)",
            ),
            // literal text that looks like a token stays text
            (vec![p("@[x](page:id)")], "@\\[x\\](page:id)"),
        ];
        for (rt, md) in cases {
            assert_eq!(to_markdown(&rt), md);
            assert!(is_markdown_editable(&rt), "{md}");
            let back = from_markdown(md);
            assert_eq!(back.len(), rt.len(), "{md}");
            for (a, b) in back.iter().zip(&rt) {
                assert_eq!(a.plain_text, b.plain_text, "{md}");
                assert_eq!(a.annotations, b.annotations, "{md}");
            }
            assert_eq!(to_markdown(&back), md);
        }
    }

    #[test]
    fn date_mentions_need_real_dates() {
        let ok = |t: &str| matches!(from_markdown(t)[0].kind, RichTextKind::Mention(_));
        assert!(ok("@[x](date:2024-02-29)"));
        assert!(ok(
            "@[x](date:2026-11-20T10:00:00.000+01:00/2026-11-20T11:00:00Z)"
        ));
        assert!(ok("@[x](date:2026-11-20/2026-11-22)"));
        for bad in [
            "2026-13-45",
            "2026-11-20xyz",
            "2025-02-29",
            "2026-11-20/2026-11-31",
            "2026-11-20T10:00",
        ] {
            assert!(!ok(&format!("@[x](date:{bad})")), "{bad}");
        }
        let d = local_date_iso(0);
        assert!(chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").is_ok());
        assert_ne!(local_date_iso(1), d);
    }

    #[test]
    fn parsed_mentions_have_the_api_shape() {
        use serde_json::json;
        let r = from_markdown("**@[Home](page:p1)**");
        assert!(r[0].annotations.bold);
        assert_eq!(
            serde_json::to_value(&r[0]).unwrap()["mention"],
            json!({ "type": "page", "page": { "id": "p1" } })
        );
        assert_eq!(serde_json::to_value(&r[0]).unwrap()["type"], "mention");
        assert_eq!(r[0].plain_text, "Home");
        let r = from_markdown("@[x](date:2026-11-20/2026-11-22)");
        assert_eq!(
            r[0].kind,
            RichTextKind::Mention(
                json!({ "type": "date", "date": { "start": "2026-11-20", "end": "2026-11-22" } })
            )
        );
        let r = from_markdown("@[x](date:2026-11-20)");
        assert_eq!(
            r[0].kind,
            RichTextKind::Mention(
                json!({ "type": "date", "date": { "start": "2026-11-20", "end": null } })
            )
        );
        assert_eq!(
            from_markdown("@[x](user:u)")[0].kind,
            RichTextKind::Mention(
                json!({ "type": "user", "user": { "object": "user", "id": "u" } })
            )
        );
        // malformed or unknown tokens are plain text
        for lit in [
            "@[x](bogus:1)",
            "@[x](page:)",
            "@[x](page:a b)",
            "@[x]",
            "@[x](date:/b)",
            "@[x](date:foo)",
            "@[x](date:2026-1-2)",
            "a @ b",
        ] {
            let r = from_markdown(lit);
            assert!(
                r.iter()
                    .all(|r| matches!(r.kind, RichTextKind::Text { .. })),
                "{lit}"
            );
        }
    }

    fn eq(t: &str) -> RichText {
        RichText {
            plain_text: t.into(),
            href: None,
            annotations: Annotations::default(),
            kind: RichTextKind::Equation {
                expression: t.into(),
            },
        }
    }

    #[test]
    fn equations_round_trip() {
        let mut beq = eq("y");
        beq.annotations.bold = true;
        let cases: Vec<Vec<RichText>> = vec![
            vec![eq("x^2")],
            vec![
                p("so "),
                eq("\\frac{a}{b}"),
                p(" and "),
                b("bold "),
                beq.clone(),
                b(" more"),
            ],
            vec![eq("a"), eq("b")],
            vec![eq("e^{i\\pi}+1=0"), p(" costs $5")],
            vec![eq("\\$x\\$")],
            vec![p("$"), eq("x"), p("$")],
        ];
        for c in cases {
            let md = to_markdown(&c);
            assert_eq!(from_markdown(&md), c, "markdown: {md}");
        }
        assert_eq!(to_markdown(&[p("$5"), eq("x")]), "\\$5$x$");
        assert_eq!(to_markdown(&[p("a"), beq]), "a**$y$**");
    }

    #[test]
    fn dollar_parsing() {
        // prices: a digit after the closing `$` keeps it text
        for lit in ["$10-$20", "$5/$6", "from $1 to $2", "$x$2"] {
            assert_eq!(from_markdown(lit), vec![p(lit)], "{lit}");
        }
        assert_eq!(from_markdown("$x$ 2"), vec![eq("x"), p(" 2")]);
        // so an equation right before a digit can't be written: read-only
        assert!(!is_markdown_editable(&[eq("x"), p("2")]));
        assert!(is_markdown_editable(&[eq("x"), p(" 2")]));
        assert!(is_markdown_editable(&[p("$10-$20")]));
        assert_eq!(from_markdown("$x$"), vec![eq("x")]);
        assert_eq!(
            from_markdown("a $x^2$ b"),
            vec![p("a "), eq("x^2"), p(" b")]
        );
        assert_eq!(from_markdown(r"\$x\$"), vec![p("$x$")]);
        // unmatched, empty, or space-padded `$` stay literal
        for lit in ["$", "a $ b", "$$", "$ x$", "$x $", "$5 and $6", "$x"] {
            assert_eq!(from_markdown(lit), vec![p(lit)], "{lit}");
        }
        // marks: bold/italic apply, a link doesn't, code spans keep `$`
        let r = from_markdown("**$x$** [$y$](u) `$z$`");
        assert!(r[0].annotations.bold && matches!(r[0].kind, RichTextKind::Equation { .. }));
        assert_eq!(r[2], eq("y"));
        assert_eq!(r[4], code("$z$"));
        // TeX backslashes are kept raw
        assert_eq!(from_markdown(r"$\alpha\$ x$"), vec![eq(r"\alpha\$ x")]);
    }

    #[test]
    fn shortcuts() {
        use TargetKind::*;
        let cases = [
            ("# a", Some((Heading1, "a"))),
            ("## a", Some((Heading2, "a"))),
            ("### a", Some((Heading3, "a"))),
            ("- a", Some((Bulleted, "a"))),
            ("* a", Some((Bulleted, "a"))),
            ("1. a", Some((Numbered, "a"))),
            ("42. ", Some((Numbered, ""))),
            ("[] a", Some((ToDo, "a"))),
            ("[ ] a", Some((ToDo, "a"))),
            ("> a", Some((Quote, "a"))),
            ("$$ x", Some((Equation, "x"))),
            ("$$", None),
            ("```", Some((Code, ""))),
            ("```x", Some((Code, "x"))),
            ("---", Some((Divider, ""))),
            ("#x", None),
            (" # a", None),
            (" # ", None),
            ("####", None),
            ("-a", None),
            ("1.a", None),
            (". a", None),
            ("--- a", None),
            ("[x] a", None),
            ("", None),
        ];
        for (input, want) in cases {
            assert_eq!(markdown_shortcut(input), want, "{input:?}");
        }
    }
}
