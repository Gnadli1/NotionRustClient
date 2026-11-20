//! Rendering: pure functions of [`State`]; no state changes here.

pub mod theme;

use iced::font::{Family, Style, Weight};
use iced::widget::text::{LineHeight, Span, Wrapping};
use iced::widget::{
    button, column, container, mouse_area, opaque, rich_text, row, rule, scrollable, space, span,
    stack, text, text_editor, text_input, Button, Column,
};
use std::ops::Range;

use iced::{Center, Element, Fill, Font, Padding, Shrink};
use notion_core::model::{RichText, RichTextKind};

use crate::app::{click, Message, State, PAGE_SCROLL_ID, SEARCH_ID, TITLE_ID};
use crate::edit::{Editing, EDITOR_ID};
use crate::keys::Keymap;
use crate::outline::{self, Marker, Row, Table};
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

/// What clicking a span does: a page mention opens the page, a link goes
/// through [`Message::LinkClick`].
fn link(t: &RichText) -> Option<Message> {
    match &t.kind {
        RichTextKind::Mention(m) if m["type"] == "page" => m["page"]["id"]
            .as_str()
            .map(|id| Message::SidebarOpen(id.to_string())),
        _ => t.href.clone().map(Message::LinkClick),
    }
}

fn styled<'a>(t: &'a RichText, c: &Colors) -> Span<'a, Message> {
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
        .link_maybe(link(t))
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
                    let spans: Vec<Span<'_, Message>> = cell
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
                    let body = container(
                        rich_text(spans)
                            .on_link_click(std::convert::identity)
                            .size(14)
                            .line_height(LINE),
                    )
                    .width(Fill);
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

/// Rows `range` of the page; `base` is their top depth (indent is relative
/// to it, so a column's content starts at its left edge). Rows inside a
/// collapsed toggle are skipped; a column_list lays its columns side by side.
fn blocks<'a>(
    s: &'a State,
    shown: &[bool],
    range: Range<usize>,
    base: usize,
    c: &Colors,
) -> Column<'a, Message> {
    let mut out: Vec<Element<'a, Message>> = Vec::new();
    let mut i = range.start;
    while i < range.end {
        let r = &s.rows[i];
        let level = r.depth.saturating_sub(base);
        if !shown[i] {
            i += 1;
            continue;
        }
        if r.marker == Marker::ColumnList {
            let cols = outline::columns(&s.rows, i)
                .into_iter()
                .map(|rg| blocks(s, shown, rg, r.depth + 2, c).width(Fill).into());
            out.push(
                container(row(cols).spacing(24).width(Fill))
                    .width(Fill)
                    .padding(Padding::ZERO.left(level as f32 * 24.0).top(4))
                    .into(),
            );
            i += 1 + outline::subtree(&s.rows, i).len();
            continue;
        }
        let open = s.open.contains(&r.block_id);
        // the sync gutter: red for a conflict/failed op, accent for a queued one
        let dot = if s.problem_ids.contains(&r.block_id) {
            Some(c.error)
        } else if s.queued_ids.contains(&r.block_id) {
            Some(c.accent)
        } else {
            None
        };
        let gutter = container(dot.map(|col| text("●").size(8).color(col)))
            .width(12)
            .padding(Padding::ZERO.top(metrics(&r.marker).1 + 10.0));
        out.push(
            row![
                gutter,
                row_view(r, level, open, s.editing.as_ref(), &s.keys, c)
            ]
            .into(),
        );
        i += 1;
    }
    column(out).spacing(2)
}

/// A row's text size and the space above it.
fn metrics(m: &Marker) -> (u32, f32) {
    match m {
        Marker::Heading(1) => (30, 24.0),
        Marker::Heading(2) => (24, 18.0),
        Marker::Heading(3) => (20, 12.0),
        Marker::Equation => (20, 0.0),
        Marker::Code(_) => (14, 0.0),
        _ => (16, 0.0),
    }
}

/// One row; `level` is its indent, `open` whether its toggle is expanded.
fn row_view<'a>(
    r: &'a Row,
    level: usize,
    open: bool,
    editing: Option<&'a Editing>,
    keys: &'a Keymap,
    c: &Colors,
) -> Element<'a, Message> {
    let heading = matches!(r.marker, Marker::Heading(_));
    let (size, above) = metrics(&r.marker);
    // indent and the space above headings sit outside the hover box
    let outer = Padding::ZERO.left(level as f32 * 24.0).top(above);
    let link = click(r).is_some();
    match &r.marker {
        Marker::Table(t) => {
            let editing = editing.filter(|e| e.block_id == r.block_id);
            return container(table_view(t, editing, keys, c))
                .padding(outer.top(4))
                .into();
        }
        Marker::Divider => {
            return container(rule::horizontal(1).style(theme::divider))
                .padding(outer.top(9).bottom(9))
                .into();
        }
        _ => {}
    }
    let checked = r.marker == Marker::Todo(true);
    // the to-do box and the triangle are their own click targets; a
    // mouse_area captures the press, so the row's Focus doesn't fire too
    let mark = |t: String, color| text(t).size(size).line_height(LINE).color(color);
    let prefix: Option<Element<'a, Message>> = match &r.marker {
        Marker::Todo(_) => Some(
            mouse_area(mark(
                r.marker.label(),
                if checked { c.accent } else { c.text },
            ))
            .on_press(Message::ToggleCheck(r.block_id.clone()))
            .into(),
        ),
        _ if r.toggle => Some(
            mouse_area(mark(if open { "▾" } else { "▸" }.into(), c.text))
                .on_press(Message::ToggleOpen(r.block_id.clone()))
                .into(),
        ),
        Marker::Bullet => Some(mark(outline::bullet(r.level).into(), c.text).into()),
        Marker::Number(n) => Some(mark(outline::number(*n, r.level), c.text).into()),
        Marker::Callout(icon) => Some(text(icon.clone()).size(size).line_height(LINE).into()),
        _ => None,
    };
    let has_prefix = prefix.is_some();
    let with_prefix = |body: Element<'a, Message>| -> Element<'a, Message> {
        match prefix {
            Some(p) => row![p, body].spacing(6).into(),
            None => body,
        }
    };
    let body: Element<'a, Message> = if let Some(e) = editing.filter(|e| e.block_id == r.block_id) {
        let body = with_prefix(editor(e, keys, size, heading));
        let body: Element<_> = match e.menu.as_ref() {
            None => body,
            Some(m) => column![body, slash_menu(e, m, c)].spacing(4).into(),
        };
        container(body).padding(BLOCK_PAD).width(Fill).into()
    } else {
        let mut spans: Vec<Span<'_, Message>> = Vec::new();
        let label = r.marker.label();
        // placeholders, cards and links keep their text label
        if !has_prefix && !label.is_empty() && !matches!(r.marker, Marker::Quote | Marker::Code(_))
        {
            let sep = if r.marker == Marker::TableOfContents {
                "\n"
            } else {
                " "
            };
            let color = if link { c.accent } else { c.secondary };
            spans.push(span(format!("{label}{sep}")).color(color));
        }
        let code = matches!(r.marker, Marker::Code(_));
        spans.extend(r.rich_text.iter().map(|t| {
            let mut sp = styled(t, c);
            if heading {
                sp.font = Some(Font {
                    weight: Weight::Bold,
                    ..sp.font.unwrap_or_default()
                });
            }
            if code {
                sp.font = Some(Font::MONOSPACE);
            }
            if checked {
                sp = sp.strikethrough(true).color(c.secondary);
            }
            sp
        }));
        let content = with_prefix(
            rich_text(spans)
                .on_link_click(std::convert::identity)
                .size(size)
                .line_height(LINE)
                .into(),
        );
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
        match msg {
            Some(msg) => button(content)
                .on_press(msg)
                .padding(BLOCK_PAD)
                .width(Fill)
                .style(theme::item(false))
                .into(),
            None => content.padding(BLOCK_PAD).into(),
        }
    };
    let body: Element<_> = match &r.marker {
        Marker::Quote => row![
            rule::vertical(3).style(theme::quote_bar),
            container(body).padding(Padding::ZERO.left(11)).width(Fill)
        ]
        .width(Fill)
        // the bar is Fill-height; Shrink keeps the row its content's height
        .height(Shrink)
        .into(),
        Marker::Callout(_) => container(body)
            .padding([12, 14])
            .width(Fill)
            .style(theme::tinted)
            .into(),
        Marker::Code(lang) => {
            container(column![text(lang.clone()).size(12).color(c.secondary), body].spacing(4))
                .padding([12, 14])
                .width(Fill)
                .style(theme::tinted)
                .into()
        }
        Marker::LinkCard { .. } => container(body)
            .padding(3)
            .width(Fill)
            .style(theme::bordered)
            .into(),
        _ => body,
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

/// Notion's sidebar: the Search row, then the page tree.
fn sidebar(s: &State) -> Element<'_, Message> {
    let search = button(text("🔍  Search").size(14))
        .on_press(Message::FocusSearch)
        .style(theme::item(false))
        .padding([4, 8])
        .width(Fill);
    let mut side = column![search].spacing(1).padding(8);
    for r in crate::sidebar::rows(&s.tree, &s.tree_open) {
        let e = r.entry;
        // the arrow is its own click target: the mouse_area takes the press
        let arrow = text(match (r.has_children, r.expanded) {
            (false, _) => "",
            (true, false) => "▸",
            (true, true) => "▾",
        })
        .size(12)
        .width(16);
        let arrow: Element<_> = if r.has_children {
            mouse_area(arrow)
                .on_press(Message::SidebarExpand(e.id.clone()))
                .into()
        } else {
            arrow.into()
        };
        let icon = text(e.icon.as_deref().unwrap_or("📄")).size(14).width(22);
        let current = s.current.as_ref() == Some(&e.id);
        side = side.push(
            button(row![arrow, icon, text(untitled(&e.title)).size(14)].spacing(2))
                .on_press(Message::SidebarOpen(e.id.clone()))
                .style(theme::page_item(current))
                .padding(Padding::from([3, 8]).left(4.0 + r.depth as f32 * 14.0))
                .width(Fill),
        );
    }
    container(scrollable(side))
        .width(240)
        .height(Fill)
        .style(theme::sidebar)
        .into()
}

/// The tabs across the top: icon, title and × each, sharing the width.
fn tab_bar(s: &State) -> Element<'_, Message> {
    let tabs = s.tabs.iter().enumerate().map(|(i, t)| {
        let active = i == s.active;
        // the active tab's slot is stale; its live state is in `s`
        let page = if active {
            s.current.as_deref()
        } else {
            t.page.as_deref()
        };
        let entry = page.and_then(|id| s.tree.iter().find(|e| e.id == id));
        let (icon, title) = match page {
            None => ("", "New tab"),
            Some(_) => (
                entry.and_then(|e| e.icon.as_deref()).unwrap_or("📄"),
                if active {
                    untitled(&s.title)
                } else {
                    untitled(entry.map_or(t.title.as_str(), |e| e.title.as_str()))
                },
            ),
        };
        // long titles are cut off at the tab's edge (iced has no ellipsis)
        let label = container(text(title).size(13).wrapping(Wrapping::None))
            .width(Fill)
            .clip(true);
        // its own click target: the mouse_area takes the press
        let close = mouse_area(text("×").size(14)).on_press(Message::CloseTab(i));
        let body = row![text(icon).size(13), label, close]
            .spacing(6)
            .align_y(Center);
        let tab = button(body)
            .on_press(Message::SwitchTab(i))
            .style(theme::tab(active))
            .padding([0, 10])
            .width(Fill)
            .height(Fill);
        container(mouse_area(tab).on_middle_press(Message::CloseTab(i)))
            .width(Fill)
            .max_width(240)
            .into()
    });
    container(row(tabs).spacing(4).height(Fill))
        .padding([4, 8])
        .width(Fill)
        .height(36)
        .style(theme::sidebar)
        .into()
}

/// "Parent › Child › Page" above the title; the ancestors are links.
fn breadcrumb<'a>(s: &'a State, id: &str, c: &Colors) -> Element<'a, Message> {
    let mut items: Vec<Element<'a, Message>> = Vec::new();
    for e in crate::sidebar::trail(&s.tree, id) {
        items.push(
            button(text(untitled(&e.title)).size(14).color(c.secondary))
                .on_press(Message::SidebarOpen(e.id.clone()))
                .style(theme::item(false))
                .padding([2, 4])
                .into(),
        );
        items.push(text("›").size(14).color(c.secondary).into());
    }
    items.push(
        container(text(untitled(&s.title)).size(14).color(c.secondary))
            .padding([2, 4])
            .into(),
    );
    row(items).spacing(2).align_y(Center).into()
}

/// The conflict/failed ops, over the page's bottom-left corner: click one to
/// go to it, or discard it (the remote version wins).
fn problems_panel<'a>(s: &'a State, c: &Colors) -> Element<'a, Message> {
    let items = s.problems.iter().map(|p| {
        let info = column![
            text(format!("{} · {}", p.kind, p.label)).size(13),
            text(&p.error).size(12).color(c.error),
        ]
        .spacing(2);
        row![
            button(info)
                .on_press(Message::OpenProblem(p.op))
                .style(theme::item(false))
                .padding([4, 8])
                .width(Fill),
            button(text("Discard").size(12))
                .on_press(Message::Discard(p.op))
                .style(theme::item(false))
                .padding([4, 8]),
        ]
        .align_y(Center)
        .into()
    });
    let panel = container(scrollable(column(items).spacing(2)))
        .max_height(300)
        .width(480)
        .padding(4)
        .style(theme::bordered);
    container(opaque(panel))
        .align_bottom(Fill)
        .width(Fill)
        .padding(Padding::ZERO.left(8).bottom(4))
        .into()
}

/// The Ctrl+K switcher: a box near the top over a dimmed page; a click on
/// the backdrop closes it.
fn search_overlay(s: &State) -> Element<'_, Message> {
    let input = text_input("Search pages…", &s.search)
        .id(SEARCH_ID)
        .on_input(Message::SearchInput)
        .on_submit(Message::SearchSubmit)
        .size(16)
        .padding([8, 10])
        .style(theme::search);
    let results = column(
        s.results
            .iter()
            .map(|(id, title)| entry(title, Message::PickResult(id.clone()), false).into()),
    )
    .spacing(1);
    let panel =
        container(column![input, container(scrollable(results)).max_height(400)].spacing(6))
            .width(560)
            .padding(8)
            .style(theme::bordered);
    // the inner opaque keeps clicks in the box from reaching the backdrop
    let backdrop = container(opaque(panel))
        .center_x(Fill)
        .height(Fill)
        .padding(Padding::ZERO.top(80))
        .style(theme::backdrop);
    opaque(mouse_area(backdrop).on_press(Message::SearchEscape))
}

pub fn view(s: &State) -> Element<'_, Message> {
    let c = theme::colors(s.mode);
    let main: Element<_> = match &s.current {
        None => container(text("New tab: pick a page on the left, or search.").color(c.secondary))
            .center(Fill)
            .into(),
        Some(id) => {
            let title_line = LineHeight::Relative(1.2);
            let title: Element<_> = match &s.title_edit {
                Some(t) => text_input("Untitled", t)
                    .id(TITLE_ID)
                    .on_input(Message::TitleInput)
                    .on_submit(Message::TitleCommit)
                    .size(40)
                    .font(BOLD)
                    .line_height(title_line)
                    .padding(0)
                    .style(theme::title)
                    .into(),
                None => {
                    let t = text(&s.title).size(40).font(BOLD).line_height(title_line);
                    if s.title_editable {
                        mouse_area(t)
                            .on_press(Message::EditTitle)
                            .interaction(iced::mouse::Interaction::Text)
                            .into()
                    } else {
                        t.into()
                    }
                }
            };
            let page = column![
                // left: past the rows' sync gutter
                container(breadcrumb(s, id, c)).padding(Padding::ZERO.top(16).left(12)),
                container(title).padding(Padding::ZERO.top(40).bottom(12).left(12)),
                blocks(s, &outline::shown(&s.rows, &s.open), 0..s.rows.len(), 0, c),
            ]
            .width(Fill)
            .max_width(PAGE_WIDTH)
            .padding(Padding::ZERO.bottom(120));
            // a click beside the title (nothing else took it) commits it
            let page = mouse_area(page).on_press(Message::TitleCommit);
            scrollable(container(page).padding([0.0, MIN_SIDE]).center_x(Fill))
                .id(PAGE_SCROLL_ID)
                .on_scroll(|v| Message::Scrolled(v.absolute_offset().y))
                .height(Fill)
                .into()
        }
    };

    let [queued, conflicts, failed] = s.counts;
    let counts = format!("conflicts {conflicts} · failed {failed}");
    let counts: Element<_> = if s.problems.is_empty() {
        text(counts).size(12).color(c.secondary).into()
    } else {
        // opens the list of them
        button(text(counts).size(12).color(c.error))
            .on_press(Message::ToggleProblems)
            .style(theme::item(s.problems_open))
            .padding([0, 4])
            .into()
    };
    let mut status = String::new();
    if s.pushing() {
        status.push_str(" · pushing…");
    } else if !s.sent.is_empty() {
        status.push_str(" · syncing…");
    }
    if let Some(e) = &s.last_error {
        status.push_str(&format!(" · error: {e}"));
    }

    // the same widget tree shape whether the sidebar or overlay is shown,
    // so toggling them doesn't reset the scrollables' state
    let (side, line): (Element<_>, Element<_>) = if s.sidebar {
        (sidebar(s), rule::vertical(1).style(theme::divider).into())
    } else {
        (space().into(), space().into())
    };
    let main = container(main).width(Fill).height(Fill);
    let mut body = stack![row![side, line, main].height(Fill)];
    if s.problems_open {
        body = body.push(problems_panel(s, c));
    }
    if s.searching {
        body = body.push(search_overlay(s));
    }
    column![
        tab_bar(s),
        rule::horizontal(1).style(theme::divider),
        body,
        rule::horizontal(1).style(theme::divider),
        container(
            row![
                text(format!("queued {queued} · "))
                    .size(12)
                    .color(c.secondary),
                counts,
                text(status).size(12).color(c.secondary),
            ]
            .align_y(Center)
        )
        .padding([4, 12]),
    ]
    .into()
}
