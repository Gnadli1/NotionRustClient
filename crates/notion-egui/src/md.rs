//! LayoutJobs: read-only rich text, and live Markdown styling of the row being
//! edited (markers stay in the text, dimmed). Colours are `PLACEHOLDER`, so the
//! widget's own text colour (light or dark theme) applies.

use eframe::egui::{
    text::{LayoutJob, TextFormat},
    Color32, FontFamily, FontId, Stroke,
};
use notion_core::model::{RichText, RichTextKind};

use crate::tex::pretty_tex;

const LINK: Color32 = Color32::from_rgb(70, 130, 220);
const CODE_BG: Color32 = Color32::from_rgba_premultiplied(40, 40, 40, 40);

#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub underline: bool,
    pub link: bool,
    pub math: bool,
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

/// A cached row's rich text. `bold` is the row's base weight (headings).
pub fn rich_job(rt: &[RichText], size: f32, bold: bool) -> LayoutJob {
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
        };
        let text = match &r.kind {
            RichTextKind::Equation { expression } => {
                s.math = true;
                pretty_tex(expression).unwrap_or_else(|| expression.clone())
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
}
