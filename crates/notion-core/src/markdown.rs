//! Rich text <-> a small Markdown subset: `**bold**`, `*italic*`, `` `code` ``,
//! `~~strike~~`, `[text](url)`, `$equation$`.
//!
//! Limitations: underline and colour are dropped; mentions are written as
//! their plain text (callers should treat such text as read-only, see
//! [`is_markdown_editable`]). Inside code spans only `\` and `` ` `` are
//! escaped; in link URLs `(` and `)` are escaped too. An equation's TeX is
//! written raw: `$` must be followed by a non-space and the closing `$`
//! preceded by one, and `\$` inside it doesn't close it. Equations keep
//! bold/italic/strike; link and code marks are dropped.

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
        let (text, link, eq) = match &r.kind {
            RichTextKind::Text { content, link } => (content.clone(), link.clone(), false),
            RichTextKind::Equation { expression } => (expression.clone(), None, true),
            _ => (r.plain_text.clone(), r.href.clone(), false),
        };
        if text.is_empty() {
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
        };
        match out.last_mut() {
            Some((t, s)) if *s == style && !eq => t.push_str(&text),
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
        if style.eq {
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

enum Item {
    Ch(char),
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
    let mut push = |text: &str, code: bool, eq: bool, active: &[Mark]| {
        let mut st = Style {
            code,
            eq,
            ..Style::default()
        };
        for m in active {
            match m {
                Mark::Link(_) if eq => {}
                Mark::Link(u) => st.link = Some(u.clone()),
                Mark::Bold => st.bold = true,
                Mark::Italic => st.italic = true,
                Mark::Strike => st.strike = true,
                Mark::Code => st.code = true,
            }
        }
        match runs.last_mut() {
            Some((t, s)) if *s == st && !eq => t.push_str(text),
            _ => runs.push((text.to_string(), st)),
        }
    };
    for it in items {
        match it {
            Item::Ch(c) => push(c.encode_utf8(&mut [0; 4]), false, false, &active),
            Item::Lit(s) => push(s, false, false, &active),
            Item::Code(t) => push(&t, true, false, &active),
            Item::Eq(t) => push(&t, false, true, &active),
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
/// no mentions, and it survives `to_markdown` → `from_markdown` (an equation
/// with a bare `$`, edge spaces, or a digit right after it doesn't).
pub fn is_markdown_editable(rt: &[RichText]) -> bool {
    !rt.iter()
        .any(|r| matches!(r.kind, RichTextKind::Mention(_)))
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

    #[test]
    fn mentions_are_not_editable() {
        assert!(is_markdown_editable(&[p("x")]));
        let mut m = p("@bob");
        m.kind = RichTextKind::Mention(serde_json::json!({ "type": "user" }));
        assert!(!is_markdown_editable(&[p("x"), m]));
        assert!(is_markdown_editable(&[p("x"), eq("x^2")]));
        // TeX that `$…$` can't carry stays read-only
        for bad in ["a$", " x", "x ", "x\\"] {
            assert!(!is_markdown_editable(&[eq(bad)]), "{bad:?}");
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
