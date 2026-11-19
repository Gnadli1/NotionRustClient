# Open questions

Section A needs your decision before implementation. Section B lists API gaps: the docs are unclear on these, so the plan assumes a safe default and moves on.

## A. Decisions for you

**Q1. Moving a block — the API can't do it.**
The docs say that existing blocks cannot be moved with append-children, and that "moving … block types in the API is not currently supported". Pages can be moved (`POST /v1/pages/{id}/move`), but blocks can't.
- *Option a (recommended):* push does `MoveBlock` as copy-then-delete. It appends a copy of the block and its subtree under the new parent, then deletes the original.
  - Downsides: the block gets a **new id**, it loses comments and backlinks, and the subtree has to be recreated block by block.
  - The op fails if the subtree contains an `Unsupported` block, a `child_page` or a `child_database`, because the API can't recreate those.
  - Indent/outdent in the Phase 2 outliner will need this.
- *Option b:* keep the `MoveBlock` op in the queue, but have push always mark it `Failed: unsupported by API`. That's less code, but outliner indenting can't sync.

**Q2. `last_edited_time` is rounded to the minute.**
A 2021 changelog entry says it is "rounded down to the closest minute"; the current reference pages don't mention this either way. A strict `remote > base` check misses a remote edit made in the same minute as our base snapshot.
- *Recommended:* keep the spec's rule (`remote > base` → conflict). Also, when the timestamps are **equal**, compare the remote block's type payload with the cached `raw` we based the edit on. If they differ, it's a conflict.
  - This costs no extra request, since push already GETs the block.
- *Alternative:* the strict timestamp check only. Accept that same-minute remote edits get overwritten, and list it as a known gap.

**Q3. Does a nested block edit bump the page's `last_edited_time`?**
The docs don't say. The plan's pull skips the whole tree when the page timestamp is unchanged.
- If nested edits don't bump it, that skip is wrong and pull has to walk the tree every time. It would still skip writes for unchanged blocks.
- *Proposal:* implement the page-level skip, add a `--full` flag to `pull` that bypasses it, and verify the behaviour in the live smoke run.

**Q4. Installing the toolchain.**
Rust isn't installed. OK to run `sudo pacman -S rustup && rustup default stable`? You may prefer to run it yourself.

**Q5. Retrying 5xx.**
The docs recommend retrying 500/502/503/504 for GET and DELETE only. The spec only asks for 429/529.
- *Default:* 429/529 only, as specified. The 5xx retry is a one-line addition if you want it.

## B. API ambiguities (assumption taken)

| # | Issue | Assumption |
|---|---|---|
| 1 | The live reference pages document **2026-03-11**, not 2025-09-03 | Pin `Notion-Version: 2025-09-03`. Use the upgrade guides as the source for 2025-09-03 shapes. |
| 2 | Upgrade guide says data source query is `PATCH`; reference says `POST` | `POST`, as in your brief |
| 3 | Append position on 2025-09-03: flat `after` vs the `position` object | Send `after` |
| 4 | `archived` vs `in_trash` | Read `in_trash`, falling back to `archived`. Never send either: delete uses `DELETE`. |
| 5 | Nesting depth allowed per append request ("two levels") | Append the subtree one level per request. Only the copy-move in Q1 needs this. |
| 6 | The 2000-character rich text limit doesn't say whether it counts bytes, code points or UTF-16 units | `edit-block` splits text into runs of ≤2000 `char`s (Unicode scalar values) |
| 7 | The block type list is incomplete (`link_to_page`, `audio`, `meeting_notes`/`transcription`, …) | Treat it as an open enum; unknown types become `Unsupported` |
| 8 | Data source `properties` map: keyed by id or by name? | Keep it as raw JSON in Phase 1 |
| 9 | Search filter key: `property:"object"` vs `type` | `property:"object"`, as in the reference |
| 10 | A 503 on a write "may already be saved"; the `retry_guidance` shape isn't documented | Not retried. The op is marked `Failed`, and the user re-runs push. |
| 11 | Per-workspace rate limit value isn't published, and its `Retry-After` can exceed 60 s | Honour `Retry-After` up to `max_backoff` (60 s, configurable). If it's longer, fail with `RetriesExhausted`. |
| 12 | Data source query has its own limit of about 3 req/s, and returns at most 10,000 results | The default 3 req/s limiter covers the rate. The 10k cap is a known gap. |
