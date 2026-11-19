//! Rich text <-> a small Markdown subset: `**bold**`, `*italic*`, `` `code` ``,
//! `~~strike~~`, `[text](url)`.
//!
//! Limitations: underline and colour are dropped; mentions and equations are
//! written as their plain text (callers should treat such text as read-only,
//! see [`is_markdown_editable`]); a code run containing a backtick is written as
//! escaped plain text, so its `code` annotation is lost.

use crate::model::{Annotations, RichText, RichTextKind};

/// Notion's per-run character limit.
const MAX_RUN: usize = 2000;

#[derive(Debug, Clone, PartialEq, Default)]
struct Style {
    link: Option<String>,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
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
        let (text, link) = match &r.kind {
            RichTextKind::Text { content, link } => (content.clone(), link.clone()),
            _ => (r.plain_text.clone(), r.href.clone()),
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
            code: a.code && !text.contains('`'),
        };
        match out.last_mut() {
            Some((t, s)) if *s == style => t.push_str(&text),
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

fn push_escaped(out: &mut String, s: &str, extra: &[char]) {
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '`' | '~' | '[' | ']') || extra.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
}

fn close(out: &mut String, m: &Mark) {
    match m {
        Mark::Link(u) => {
            out.push_str("](");
            push_escaped(out, u, &['(', ')']);
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
        if style.code {
            out.push_str(&text);
        } else {
            push_escaped(&mut out, &text, &[]);
        }
    }
    while let Some(m) = open.pop() {
        close(&mut out, &m);
    }
    out
}

enum Item {
    Ch(char),
    Code(String),
    Open(Mark),
    Close,
}

pub fn from_markdown(s: &str) -> Vec<RichText> {
    let cs: Vec<char> = s.chars().collect();
    let mut items: Vec<Item> = Vec::new();
    // (mark, index of its Open item)
    let mut stack: Vec<(Mark, usize)> = Vec::new();
    let top = |st: &Vec<(Mark, usize)>| st.last().map(|(m, _)| m.clone());
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '\\' if next.is_some_and(|n| n.is_ascii_punctuation()) => {
                items.push(Item::Ch(next.unwrap()));
                i += 2;
                continue;
            }
            '`' => {
                if let Some(end) = cs[i + 1..].iter().position(|&x| x == '`') {
                    items.push(Item::Code(cs[i + 1..i + 1 + end].iter().collect()));
                    i += end + 2;
                    continue;
                }
            }
            '*' => {
                let (mark, n) = if top(&stack) == Some(Mark::Italic) {
                    (Mark::Italic, 1)
                } else if next == Some('*') {
                    (Mark::Bold, 2)
                } else {
                    (Mark::Italic, 1)
                };
                if top(&stack) == Some(mark.clone()) {
                    stack.pop();
                    items.push(Item::Close);
                } else {
                    stack.push((mark.clone(), items.len()));
                    items.push(Item::Open(mark));
                }
                i += n;
                continue;
            }
            '~' if next == Some('~') => {
                if top(&stack) == Some(Mark::Strike) {
                    stack.pop();
                    items.push(Item::Close);
                } else {
                    stack.push((Mark::Strike, items.len()));
                    items.push(Item::Open(Mark::Strike));
                }
                i += 2;
                continue;
            }
            '[' => {
                stack.push((Mark::Link(String::new()), items.len()));
                items.push(Item::Open(Mark::Link(String::new())));
                i += 1;
                continue;
            }
            ']' if matches!(top(&stack), Some(Mark::Link(_))) && next == Some('(') => {
                if let Some((url, end)) = parse_url(&cs, i + 2) {
                    let (_, at) = stack.pop().unwrap();
                    items[at] = Item::Open(Mark::Link(url));
                    items.push(Item::Close);
                    i = end;
                    continue;
                }
            }
            _ => {}
        }
        items.push(Item::Ch(c));
        i += 1;
    }
    // Unmatched openers are literal text.
    for (m, at) in stack {
        let lit = open_str(&m);
        items[at] = Item::Code(String::new()); // placeholder, replaced below
        items.splice(at..=at, lit.chars().map(Item::Ch));
    }

    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut active: Vec<Mark> = Vec::new();
    let mut push = |c: &str, st: Style| match runs.last_mut() {
        Some((t, s)) if *s == st => t.push_str(c),
        _ => runs.push((c.to_string(), st)),
    };
    let style_of = |active: &[Mark]| {
        let mut st = Style::default();
        for m in active {
            match m {
                Mark::Link(u) => st.link = Some(u.clone()),
                Mark::Bold => st.bold = true,
                Mark::Italic => st.italic = true,
                Mark::Strike => st.strike = true,
                Mark::Code => st.code = true,
            }
        }
        st
    };
    for it in items {
        match it {
            Item::Ch(c) => push(c.encode_utf8(&mut [0; 4]), style_of(&active)),
            Item::Code(t) if !t.is_empty() => {
                let mut st = style_of(&active);
                st.code = true;
                push(&t, st);
            }
            Item::Code(_) => {}
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
                kind: RichTextKind::Text {
                    content: t,
                    link: st.link,
                },
            })
            .collect(),
    )
}

/// URL after `](`, honouring backslash escapes; returns (url, index past `)`).
fn parse_url(cs: &[char], mut i: usize) -> Option<(String, usize)> {
    let mut url = String::new();
    while i < cs.len() {
        match cs[i] {
            '\\' if i + 1 < cs.len() => {
                url.push(cs[i + 1]);
                i += 2;
            }
            ')' => return Some((url, i + 1)),
            c => {
                url.push(c);
                i += 1;
            }
        }
    }
    None
}

/// False if any run is a mention or equation (Markdown would flatten it).
pub fn is_markdown_editable(rt: &[RichText]) -> bool {
    rt.iter()
        .all(|r| matches!(r.kind, RichTextKind::Text { .. }))
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

    #[test]
    fn round_trips() {
        let mut underlined = rt("u", None, true, false, false, false);
        underlined.annotations.underline = true;
        underlined.annotations.color = "red".into();
        let cases: Vec<Vec<RichText>> = vec![
            vec![p("plain text")],
            vec![p("a "), rt("bold", None, true, false, false, false), p(" b")],
            vec![rt("bi", None, true, true, false, false)],
            vec![
                p("x "),
                rt("bold", None, true, false, false, false),
                rt("both", None, true, true, false, false),
                rt("bold", None, true, false, false, false),
            ],
            vec![
                rt("go ", Some("http://e.com/a_(b)"), false, false, false, false),
                rt("here", Some("http://e.com/a_(b)"), true, false, false, false),
            ],
            vec![rt("a*b", None, false, false, false, true), p(" x")],
            vec![p(r"lit * [x] \ ` ~ ~~ ** end")],
            vec![
                rt("b", None, true, false, false, false),
                rt("i", None, false, true, false, false),
            ],
            vec![
                rt("i", None, false, true, false, false),
                rt("bi", None, true, true, false, false),
            ],
            vec![
                rt("bi", None, true, true, false, false),
                rt("i", None, false, true, false, false),
            ],
            vec![
                rt("lb", Some("u"), true, false, false, false),
                rt("b", None, true, false, false, false),
            ],
            vec![
                rt("s", None, false, false, true, false),
                rt("si", None, false, true, true, false),
                rt("c", None, true, false, true, true),
            ],
            vec![p("a"), p("b"), underlined],
        ];
        let key = |v: &[RichText]| normalize(v);
        for c in cases {
            let md = to_markdown(&c);
            let back = from_markdown(&md);
            assert_eq!(key(&back), key(&c), "markdown: {md}");
            for r in &back {
                let RichTextKind::Text { content, link } = &r.kind else {
                    panic!()
                };
                assert_eq!((&r.plain_text, &r.href), (content, link));
            }
        }
    }

    #[test]
    fn parses_exact_runs() {
        assert_eq!(
            from_markdown("a **b *c* d** e"),
            vec![
                p("a "),
                rt("b ", None, true, false, false, false),
                rt("c", None, true, true, false, false),
                rt(" d", None, true, false, false, false),
                p(" e"),
            ]
        );
        assert_eq!(
            from_markdown("[x](http://y)"),
            vec![rt("x", Some("http://y"), false, false, false, false)]
        );
        assert_eq!(
            from_markdown("`a*b`"),
            vec![rt("a*b", None, false, false, false, true)]
        );
        assert_eq!(from_markdown("**unclosed"), vec![p("**unclosed")]);
        assert_eq!(from_markdown(r"\*x\* [y"), vec![p("*x* [y")]);
        assert_eq!(
            from_markdown("~~s~~ `open"),
            vec![rt("s", None, false, false, true, false), p(" `open")]
        );
    }

    #[test]
    fn backtick_code_falls_back_and_long_runs_split() {
        let c = rt("a`b", None, false, false, false, true);
        assert_eq!(from_markdown(&to_markdown(&[c])), vec![p("a`b")]);
        let long = from_markdown(&"x".repeat(4500));
        let lens: Vec<usize> = long.iter().map(|r| r.plain_text.len()).collect();
        assert_eq!(lens, [2000, 2000, 500]);
    }

    #[test]
    fn mentions_are_not_editable() {
        assert!(is_markdown_editable(&[p("x")]));
        let mut m = p("@bob");
        m.kind = RichTextKind::Mention(serde_json::json!({ "type": "user" }));
        assert!(!is_markdown_editable(&[p("x"), m]));
    }
}
