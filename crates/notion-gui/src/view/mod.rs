//! Rendering: pure functions of [`State`]; no state changes here.

pub mod theme;

use iced::font::{Family, Style, Weight};
use iced::widget::text::{LineHeight, Span};
use iced::widget::{
    button, column, container, mouse_area, rich_text, row, rule, scrollable, span, text,
    text_editor, text_input, Button,
};
use iced::{Element, Fill, Font, Padding};
use notion_core::model::{RichText, RichTextKind};

use crate::app::{click, Message, State, SEARCH_ID};
use crate::edit::{Editing, EDITOR_ID};
use crate::keys::Keymap;
use crate::outline::{Marker, Row, Table};
use crate::slash::{self, Menu};
use theme::Colors;

/// Notion's default page width.
const PAGE_WIDTH: f32 = 708.0;
/// Page side margin on narrow windows; wide ones get more from centring.
const MIN_SIDE: f32 = 24.0;
const LINE: LineHeight = LineHeight::Relative(1.5);
/// Around every block (the hover box), so the editor lines up with the text.
const BLOCK_PAD: Padding = Padding {
    top: 3.0,
    bottom: 3.0,
    left: 2.0,
    right: 2.0,
};

const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

pub(crate) fn untitled(t: &str) -> &str {
    if t.is_empty() {
        "Untitled"
    } else {
        t
    }
}

fn styled<'a>(t: &'a RichText, c: &Colors) -> Span<'a, ()> {
    if let RichTextKind::Equation { expression } = &t.kind {
        // Unicode pretty-print in italic serif; the TeX source in monospace if it can't
        return match crate::tex::pretty_tex(expression) {
            Some(p) => span(p).font(Font {
                family: Family::Serif,
                style: Style::Italic,
                ..Font::DEFAULT
            }),
            None => span(expression.as_str()).font(Font::MONOSPACE),
        };
    }
    let a = &t.annotations;
    let mut font = if a.code {
        Font::MONOSPACE
    } else {
        Font::DEFAULT
    };
    if a.bold {
        font.weight = Weight::Bold;
    }
    if a.italic {
        font.style = Style::Italic;
    }
    span(t.plain_text.as_str())
        .font(font)
        .strikethrough(a.strikethrough)
        .underline(a.underline || t.href.is_some())
        .color_maybe(t.href.as_ref().map(|_| c.accent))
}

fn editor<'a>(e: &'a Editing, keys: &'a Keymap, size: u32, bold: bool) -> Element<'a, Message> {
    text_editor(&e.content)
        .id(EDITOR_ID)
        .on_action(Message::EditorAction)
        .key_binding(move |kp| crate::edit::binding(kp, keys))
        .font(if e.code {
            Font::MONOSPACE
        } else if bold {
            BOLD
        } else {
            Font::DEFAULT
        })
        .size(size)
        .line_height(LINE)
        .padding(0)
        .style(theme::editor)
        .into()
}

/// A table as a grid; header row / column in bold, the edited cell as an editor.
fn table_view<'a>(
    t: &'a Table,
    editing: Option<&'a Editing>,
    keys: &'a Keymap,
    c: &Colors,
) -> Element<'a, Message> {
    let rows = t.rows.iter().enumerate().map(|(i, tr)| {
        let cells = tr.cells.iter().enumerate().map(|(j, cell)| {
            let bold = (t.column_header && i == 0) || (t.row_header && j == 0);
            let edited = editing.filter(|e| {
                e.cell
                    .as_ref()
                    .is_some_and(|(id, col)| *id == tr.id && *col == j)
            });
            let body: Element<_> = match edited {
                Some(e) => editor(e, keys, 14, false),
                None => {
                    let spans: Vec<Span<'_, ()>> = cell
                        .rich_text
                        .iter()
                        .map(|rt| {
                            let mut sp = styled(rt, c);
                            if bold {
                                sp.font = Some(Font {
                                    weight: Weight::Bold,
                                    ..sp.font.unwrap_or_default()
                                });
                            }
                            sp
                        })
                        .collect();
                    let body = container(rich_text(spans).size(14).line_height(LINE)).width(Fill);
                    if cell.editable {
                        mouse_area(body)
                            .on_press(Message::FocusCell(tr.id.clone(), j))
                            .into()
                    } else {
                        body.into()
                    }
                }
            };
            container(body)
                .padding([4, 8])
                .width(Fill)
                .style(theme::bordered)
                .into()
        });
        row(cells).into()
    });
    column(rows).into()
}

fn row_view<'a>(
    r: &'a Row,
    editing: Option<&'a Editing>,
    keys: &'a Keymap,
    c: &Colors,
) -> Element<'a, Message> {
    let heading = matches!(r.marker, Marker::Heading(_));
    let (size, above) = match r.marker {
        Marker::Heading(1) => (30, 24.0),
        Marker::Heading(2) => (24, 18.0),
        Marker::Heading(3) => (20, 12.0),
        Marker::Equation => (20, 0.0),
        _ => (16, 0.0),
    };
    // indent and the space above headings sit outside the hover box
    let outer = Padding::ZERO.left(r.depth as f32 * 24.0).top(above);
    let link = click(r).is_some();
    if let Marker::Table(t) = &r.marker {
        let editing = editing.filter(|e| e.block_id == r.block_id);
        return container(table_view(t, editing, keys, c))
            .padding(outer.top(4))
            .into();
    }
    let label = r.marker.label();
    if let Some(e) = editing.filter(|e| e.block_id == r.block_id) {
        let editor = editor(e, keys, size, heading);
        let body: Element<_> = if label.is_empty() {
            editor
        } else {
            let marker = mouse_area(text(label).size(size).line_height(LINE).color(c.secondary))
                .on_press(Message::ToggleCheck(r.block_id.clone()));
            row![marker, editor].spacing(6).into()
        };
        let body: Element<_> = match e.menu.as_ref() {
            None => body,
            Some(m) => column![body, slash_menu(e, m, c)].spacing(4).into(),
        };
        return container(container(body).padding(BLOCK_PAD))
            .padding(outer)
            .into();
    }
    let mut spans: Vec<Span<'_, ()>> = Vec::new();
    // to-do box is its own click target; it captures the press, so the
    // row's Focus doesn't fire too
    let todo = matches!(r.marker, Marker::Todo(_)).then(|| {
        mouse_area(
            text(label.clone())
                .size(size)
                .line_height(LINE)
                .color(c.secondary),
        )
        .on_press(Message::ToggleCheck(r.block_id.clone()))
    });
    if todo.is_none() && !label.is_empty() {
        let sep = if matches!(r.marker, Marker::Code(_) | Marker::TableOfContents) {
            "\n"
        } else {
            " "
        };
        let color = if link { c.accent } else { c.secondary };
        spans.push(span(format!("{label}{sep}")).color(color));
    }
    spans.extend(r.rich_text.iter().map(|t| {
        let mut sp = styled(t, c);
        if heading {
            sp.font = Some(Font {
                weight: Weight::Bold,
                ..sp.font.unwrap_or_default()
            });
        }
        sp
    }));
    let text = rich_text(spans).size(size).line_height(LINE);
    let content: Element<_> = match todo {
        Some(t) => row![t, text].spacing(6).into(),
        None => text.into(),
    };
    let mut content = container(content).width(Fill);
    if r.marker == Marker::Equation {
        content = content.center_x(Fill);
    }
    // a button only for its hover state; it fires on release, not press
    let msg = if r.editable {
        Some(Message::Focus(r.block_id.clone()))
    } else {
        click(r)
    };
    let body: Element<_> = match msg {
        Some(msg) => button(content)
            .on_press(msg)
            .padding(BLOCK_PAD)
            .width(Fill)
            .style(theme::item(false))
            .into(),
        None => content.padding(BLOCK_PAD).into(),
    };
    container(body).padding(outer).into()
}

/// The `/` menu, drawn in the outline right under the edited row.
fn slash_menu<'a>(e: &Editing, m: &Menu, c: &Colors) -> Element<'a, Message> {
    let (line, col) = e.pos();
    let buf = e.content.text();
    let q = m.query(&buf, line, col).unwrap_or_default();
    let items = slash::filter(q).into_iter().enumerate().map(|(i, it)| {
        button(
            row![
                text(it.name).size(14).width(140),
                text(it.hint).size(12).color(c.secondary)
            ]
            .spacing(8),
        )
        .on_press(Message::SlashPick(i))
        .style(theme::item(i == m.selected))
        .padding([4, 8])
        .width(Fill)
        .into()
    });
    container(column(items).spacing(2))
        .style(theme::bordered)
        .padding(4)
        .max_width(360)
        .into()
}

/// A sidebar entry: a page or search result.
fn entry<'a>(title: &'a str, msg: Message, selected: bool) -> Button<'a, Message> {
    button(text(untitled(title)).size(14))
        .on_press(msg)
        .style(theme::item(selected))
        .padding([4, 8])
        .width(Fill)
}

pub fn view(s: &State) -> Element<'_, Message> {
    let c = theme::colors(s.mode);
    let mut side = column![text_input("Search… (Ctrl+K)", &s.search)
        .id(SEARCH_ID)
        .on_input(Message::SearchInput)
        .on_submit(Message::SearchSubmit)
        .size(14)
        .padding([6, 8])
        .style(theme::search)]
    .spacing(2)
    .padding(8);
    for (id, title) in &s.results {
        side = side.push(entry(title, Message::PickResult(id.clone()), false));
    }
    if !s.results.is_empty() {
        side = side.push(container(text("Cached").size(12).color(c.secondary)).padding([6, 8]));
    }
    for (id, title) in &s.pages {
        let current = s.current.as_ref() == Some(id);
        side = side.push(entry(title, Message::Open(id.clone()), current));
    }
    let side = container(scrollable(side))
        .width(240)
        .height(Fill)
        .style(theme::sidebar);

    let main: Element<_> = match &s.current {
        None => container(text("Pick a page on the left, or search.").color(c.secondary))
            .center(Fill)
            .into(),
        Some(_) => {
            let title = text(&s.title)
                .size(40)
                .font(BOLD)
                .line_height(LineHeight::Relative(1.2));
            let page = column![
                container(title).padding(Padding::ZERO.top(64).bottom(12)),
                column(
                    s.rows
                        .iter()
                        .map(|r| row_view(r, s.editing.as_ref(), &s.keys, c))
                )
                .spacing(2),
            ]
            .width(Fill)
            .max_width(PAGE_WIDTH)
            .padding(Padding::ZERO.bottom(120));
            scrollable(container(page).padding([0.0, MIN_SIDE]).center_x(Fill))
                .height(Fill)
                .into()
        }
    };

    let [queued, conflicts, failed] = s.counts;
    let mut status = format!("queued {queued} · conflicts {conflicts} · failed {failed}");
    if s.pushing() {
        status.push_str(" · pushing…");
    } else if !s.sent.is_empty() {
        status.push_str(" · syncing…");
    }
    if let Some(e) = &s.last_error {
        status.push_str(&format!(" · error: {e}"));
    }

    column![
        row![
            side,
            rule::vertical(1).style(theme::divider),
            container(main).width(Fill).height(Fill)
        ]
        .height(Fill),
        rule::horizontal(1).style(theme::divider),
        container(text(status).size(12).color(c.secondary)).padding([4, 12]),
    ]
    .into()
}
