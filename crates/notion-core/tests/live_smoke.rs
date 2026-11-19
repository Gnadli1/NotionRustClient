//! Read-only smoke test against real Notion. Run with:
//! NOTION_TOKEN=... NOTION_TEST_PAGE_ID=<page id> cargo test -p notion-core --test live_smoke -- --ignored

use notion_core::api::Client;
use notion_core::store::Store;
use notion_core::sync::pull_page;

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn pull_real_page() {
    let (Ok(_), Ok(page_id)) = (
        std::env::var("NOTION_TOKEN"),
        std::env::var("NOTION_TEST_PAGE_ID"),
    ) else {
        eprintln!("skipping: set NOTION_TOKEN and NOTION_TEST_PAGE_ID");
        return;
    };
    // the cache is keyed by Notion's dashed lowercase id
    let h: String = page_id
        .to_lowercase()
        .chars()
        .filter(|c| *c != '-')
        .collect();
    assert_eq!(h.len(), 32, "NOTION_TEST_PAGE_ID must be a page id");
    let page_id = format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    );
    let client = Client::from_env().unwrap();
    let store = Store::open_in_memory().unwrap();

    let first = pull_page(&client, &store, &page_id, true).await.unwrap();
    let page = store
        .get_page(&page_id)
        .unwrap()
        .expect("page cached after pull");
    let has_blocks = !store.children(&page.id).unwrap().is_empty();
    assert!(first.fetched > 0 || !has_blocks, "{first:?}");

    if first.fetched > 0 {
        let second = pull_page(&client, &store, &page_id, false).await.unwrap();
        assert!(second.skipped, "{second:?}");
    }
}
