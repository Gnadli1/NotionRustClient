//! Test-harness CLI over notion-core. Token comes from NOTION_TOKEN and is never printed.

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use notion_core::api::Client;
use notion_core::model::{plain_text, Block, BlockKind, SearchResult};
use notion_core::ops::OpStatus;
use notion_core::store::Store;
use notion_core::sync;

#[derive(Parser)]
#[command(about = "Notion client test harness")]
struct Cli {
    /// SQLite cache path [default: $XDG_DATA_HOME/notion-client/cache.db]
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Search pages and data sources
    Search { query: String },
    /// Fetch a page and its blocks into the cache
    Pull {
        page_id: String,
        /// Re-fetch even if the page looks unchanged
        #[arg(long)]
        full: bool,
    },
    /// Print a cached page (offline)
    Cat { page_id: String },
    /// Queue a text edit for a block (offline)
    EditBlock { block_id: String, text: String },
    /// Send queued ops to Notion
    Push,
    /// List queued, conflicting and failed ops (offline)
    Status,
}

fn db_path(arg: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".local/share")
        }
    };
    Ok(base.join("notion-client/cache.db"))
}

fn open_store(arg: Option<PathBuf>) -> anyhow::Result<Store> {
    let path = db_path(arg)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(Store::open(&path)?)
}

/// Last 32 hex chars of a dashed id, undashed id or Notion URL, as 8-4-4-4-12.
fn normalize_id(arg: &str) -> anyhow::Result<String> {
    // drop ?query / #fragment first: "?pvs=4" would otherwise contribute hex digits
    let path = arg
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let hex: Vec<char> = path.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() < 32 {
        bail!("invalid id {arg:?}: need 32 hex characters");
    }
    let h: String = hex[hex.len() - 32..].iter().collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

fn render_page(store: &Store, page_id: &str) -> anyhow::Result<String> {
    let page = store
        .get_page(page_id)?
        .context("page not cached; run pull first")?;
    let mut out = format!("# {}\n\n", page.title());
    render_blocks(store, page_id, 0, &mut out)?;
    Ok(out)
}

fn render_blocks(
    store: &Store,
    parent_id: &str,
    depth: usize,
    out: &mut String,
) -> anyhow::Result<()> {
    for b in store.children(parent_id)? {
        render_block(&b, depth, out);
        render_blocks(store, &b.id, depth + 1, out)?;
    }
    Ok(())
}

fn render_block(b: &Block, depth: usize, out: &mut String) {
    let ind = "  ".repeat(depth);
    let text = b
        .kind
        .text()
        .map(|t| plain_text(&t.rich_text))
        .unwrap_or_default();
    let marker = match &b.kind {
        BlockKind::Heading1(_) => "# ",
        BlockKind::Heading2(_) => "## ",
        BlockKind::Heading3(_) => "### ",
        BlockKind::BulletedListItem(_) => "- ",
        BlockKind::NumberedListItem(_) => "1. ",
        BlockKind::ToDo { checked: true, .. } => "[x] ",
        BlockKind::ToDo { .. } => "[ ] ",
        BlockKind::Toggle(_) => "▸ ",
        BlockKind::Quote(_) => "> ",
        BlockKind::Callout(_) => "! ",
        _ => "",
    };
    let line = match &b.kind {
        // Code lines keep their own indentation, so continuation lines get only the block indent.
        BlockKind::Code { language, .. } => format!("```{language}\n{text}\n```"),
        BlockKind::Divider => "---".into(),
        BlockKind::Equation { expression } => format!("$${expression}$$"),
        BlockKind::ChildPage { title } => format!("[page: {title}]"),
        BlockKind::ChildDatabase { title } => format!("[database: {title}]"),
        BlockKind::Unsupported { .. } => format!("[{}]", b.kind.type_name()),
        _ => format!("{marker}{text}"),
    };
    // Continuation lines line up under the text, after the marker.
    let cont = format!("\n{ind}{}", " ".repeat(marker.chars().count()));
    out.push_str(&format!("{ind}{}\n", line.replace('\n', &cont)));
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Search { query } => {
            let client = Client::from_env()?;
            for r in client.search(&query).await? {
                match r {
                    SearchResult::Page(p) => println!("page  {}  {}", p.id, p.title()),
                    SearchResult::DataSource(d) => println!("data_source  {}  {}", d.id, d.title),
                }
            }
        }
        Cmd::Pull { page_id, full } => {
            let id = normalize_id(&page_id)?;
            let client = Client::from_env()?;
            let store = open_store(cli.db)?;
            let r = sync::pull_page(&client, &store, &id, full).await?;
            if r.skipped {
                println!("unchanged (skipped)");
            } else {
                println!("fetched {}, updated {}", r.fetched, r.updated);
            }
        }
        Cmd::Cat { page_id } => {
            let id = normalize_id(&page_id)?;
            print!("{}", render_page(&open_store(cli.db)?, &id)?);
        }
        Cmd::EditBlock { block_id, text } => {
            let id = normalize_id(&block_id)?;
            let op = sync::local_edit_text(&open_store(cli.db)?, &id, &text)?;
            println!("queued op {op}");
        }
        Cmd::Push => {
            let client = Client::from_env()?;
            let r = sync::push(&client, &open_store(cli.db)?).await?;
            println!(
                "applied {}, conflicts {}, failed {}",
                r.applied, r.conflicts, r.failed
            );
        }
        Cmd::Status => {
            let store = open_store(cli.db)?;
            let mut ops = store.queued_ops()?;
            ops.extend(store.ops_by_status(OpStatus::Conflict)?);
            ops.extend(store.ops_by_status(OpStatus::Failed)?);
            if ops.is_empty() {
                println!("no pending ops");
            }
            for op in ops {
                let kind = serde_json::to_value(&op.kind)?;
                let status = match op.status {
                    OpStatus::Queued => "queued",
                    OpStatus::Conflict => "conflict",
                    OpStatus::Failed => "failed",
                };
                let line = format!(
                    "{status}  #{}  {}  {}  {}",
                    op.id,
                    kind["kind"].as_str().unwrap_or("?"),
                    op.kind.target_id(),
                    op.error.as_deref().unwrap_or("")
                );
                println!("{}", line.trim_end());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notion_core::model::Page;
    use serde_json::{json, Value};

    const PAGE: &str = "0123456789abcdef0123456789abcdef";
    const PID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    const T: &str = "2025-01-01T00:00:00.000Z";

    fn rt(s: &str) -> Value {
        json!([{ "type": "text", "text": { "content": s }, "plain_text": s, "annotations": {} }])
    }

    fn blk(id: &str, parent: &str, t: &str, payload: Value) -> Block {
        serde_json::from_value(json!({
            "object": "block", "id": id, "parent": { "type": "block_id", "block_id": parent },
            "type": t, t: payload, "has_children": false, "in_trash": false,
            "created_time": T, "last_edited_time": T,
        }))
        .unwrap()
    }

    #[test]
    fn renders_cached_page() {
        let store = Store::open_in_memory().unwrap();
        let page: Page = serde_json::from_value(json!({
            "object": "page", "id": PID, "parent": { "type": "workspace", "workspace": true },
            "created_time": T, "last_edited_time": T, "in_trash": false, "url": "https://notion.so/x",
            "properties": { "Name": { "id": "title", "type": "title", "title": rt("My Page") } },
        }))
        .unwrap();
        store.upsert_page(&page).unwrap();
        store
            .replace_children(
                PID,
                PID,
                &[
                    blk("h", PID, "heading_2", json!({ "rich_text": rt("Intro") })),
                    blk("p", PID, "paragraph", json!({ "rich_text": rt("Hello") })),
                    blk(
                        "b",
                        PID,
                        "bulleted_list_item",
                        json!({ "rich_text": rt("item") }),
                    ),
                    blk(
                        "t",
                        PID,
                        "to_do",
                        json!({ "rich_text": rt("done"), "checked": true }),
                    ),
                    blk("d", PID, "divider", json!({})),
                    blk("u", PID, "image", json!({ "type": "external" })),
                ],
            )
            .unwrap();
        store
            .replace_children(
                PID,
                "b",
                &[
                    blk(
                        "n",
                        "b",
                        "numbered_list_item",
                        json!({ "rich_text": rt("nested") }),
                    ),
                    blk(
                        "c",
                        "b",
                        "code",
                        json!({ "rich_text": rt("let x = 1;\n  y();"), "language": "rust" }),
                    ),
                    blk(
                        "m",
                        "b",
                        "bulleted_list_item",
                        json!({ "rich_text": rt("first\nsecond") }),
                    ),
                ],
            )
            .unwrap();

        let expected = "# My Page\n\n## Intro\nHello\n- item\n  1. nested\n  ```rust\n  let x = 1;\n    y();\n  ```\n  - first\n    second\n[x] done\n---\n[image]\n";
        assert_eq!(render_page(&store, PID).unwrap(), expected);
        assert!(render_page(&store, "ffffffff-ffff-ffff-ffff-ffffffffffff").is_err());
    }

    #[test]
    fn normalizes_ids() {
        assert_eq!(normalize_id(PID).unwrap(), PID);
        assert_eq!(normalize_id(PAGE).unwrap(), PID);
        assert_eq!(normalize_id(&PAGE.to_uppercase()).unwrap(), PID);
        let url = format!("https://www.notion.so/My-Page-{PAGE}?pvs=4");
        assert_eq!(normalize_id(&url).unwrap(), PID);
        assert!(normalize_id("0123456789abcdef").is_err());
    }
}
