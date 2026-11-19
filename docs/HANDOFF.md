# Handoff: continue Phase 2 on the MiniPC (2026-11-19)

Read `docs/phase2-plan.md` first. It is approved with all defaults: iced 0.14, manual Ctrl+S push, Tab/Shift+Tab in G3, and mention/equation blocks read-only. Phase 1 is done and verified live; see `docs/plan.md` §6–7.

## State at handoff
- Committed and reviewed: T0–T5 (Phase 1), the cli `cat` indent fix, and the Phase 2 plan.
- In this WIP commit, **not reviewed**:
  - `crates/notion-gui` skeleton: `fn main() {}`, iced 0.14 added to `[workspace.dependencies]`, crate in workspace members. It builds.
  - `crates/notion-core/src/markdown.rs`: a partial draft from the stopped C1 agent. It is **not** in `lib.rs` yet, so it isn't compiled. Review or redo it.
- Task C1 (core additions) is unfinished. Next steps: C1 → G1 → G2 → G3, each implemented by one subagent and reviewed by a separate one (max 2 rounds), with one commit per task. The gate is `cargo test` and `cargo clippy --all-targets -- -D warnings`.

## C1 spec (as briefed to the stopped agent)
**Add, and don't change existing signatures:**
- `markdown.rs`
  - `to_markdown(&[RichText]) -> String`
  - `from_markdown(&str) -> Vec<RichText>`
  - `is_markdown_editable(&[RichText]) -> bool` (false if any mention or equation)
  - Syntax: `**b**`, `*i*`, `` `c` ``, `~~s~~`, `[t](url)`. Nesting order outer→inner: link, bold, italic, strike, code. Merge adjacent runs that are equal. Drop underline and color. Backslash-escape `\ * ` ~ [ ]`. Unmatched markers stay literal. Split runs longer than 2000 chars.
- `Store`
  - `pages() -> Vec<(id, title)>`: not in trash, ordered by title.
  - `move_block_local(id, new_parent, after)`:
    - `after: None` means end.
    - Renumber positions in both the old and the new parent.
    - Patch the raw `parent`.
    - Fix `has_children` on both parents.
    - Reject cycles.
  - `busy_timeout` 5 s in `open`.
- ops
  - `update_op_kind(id, &OpKind)`
  - `queued_create_for(local_id) -> Option<Op>`
- sync
  - `local_edit_rich(store, id, Vec<RichText>) -> i64`. `local_edit_text` becomes a wrapper around it.
  - `local_delete_block(...) -> Option<i64>`
  - `local_move_block(store, id, new_parent, after) -> Option<i64>`
  - `local_set_checked(store, id, bool) -> i64`, with PATCH body `{"to_do":{"checked":b}}`.
  - For an unpushed `local-*` block, rewrite or remove its queued CreateBlock instead of queueing new ops.
  - `local_create_block` must respect `after` for its cached position.
  - A locally moved block must survive a pull before it's pushed.

## Environment notes
- Notion API token: env `NOTION_TOKEN`. It's not copied here; set it yourself, and never commit or log it.
- Live test page id: `<your-test-page-id>`. Run the live smoke test with `NOTION_TEST_PAGE_ID=<id> cargo test -p notion-core --test live_smoke -- --ignored`.
- The repo was pushed here from the laptop with `git push minipc-ts:NotionRustClient master`; there is no other remote.
