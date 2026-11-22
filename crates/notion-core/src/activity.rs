//! C5: the local Updates inbox. Notion's own inbox isn't in the API, so this
//! is built from what pulls already see: new comments, comments that mention
//! you, and pages others edited since you last opened them. "You" is the
//! integration's bot user and, if a person owns the integration, that owner
//! (`GET /v1/users/me`, saved by [`Store::set_identity`]), or the person named
//! by `NOTION_USER_ID`, which wins. Nothing here makes
//! a request. Entries dedupe by (kind, target) and start unread.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};

use crate::model::{plain_text, Comment, Page, RichTextKind};
use crate::store::{ts, Store};
use crate::Result;

pub const COMMENT: &str = "comment";
pub const MENTION: &str = "mention";
pub const PAGE: &str = "page";

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// [`COMMENT`], [`MENTION`] or [`PAGE`].
    pub kind: String,
    /// The comment id, or the page id for [`PAGE`].
    pub target: String,
    /// The page to open.
    pub page_id: String,
    /// What the comment hangs off (the page or a block); '' for [`PAGE`].
    pub parent_id: String,
    /// User id of the commenter or editor.
    pub author: String,
    pub summary: String,
    pub at: DateTime<Utc>,
    pub unread: bool,
}

/// Who "you" are: the bot, and the person who owns it (None for a
/// workspace-owned integration, which can't tell mentions of you).
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub bot: String,
    pub owner: Option<String>,
}

impl Identity {
    fn is_me(&self, id: &str) -> bool {
        id == self.bot || self.owner.as_deref() == Some(id)
    }
}

impl Store {
    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Saves who you are (from `Client::me_identity`). `env_user` (the
    /// `NOTION_USER_ID` setting) wins over the integration's owner, which an
    /// internal integration doesn't have as a person.
    pub fn set_identity(
        &self,
        bot: &str,
        owner: Option<&str>,
        env_user: Option<&str>,
    ) -> Result<()> {
        self.set_meta("bot_id", bot)?;
        self.set_meta(
            "owner_id",
            env_user.filter(|u| !u.is_empty()).or(owner).unwrap_or(""),
        )
    }

    /// None until `set_identity` ran. Recording needs a person, see
    /// [`person`](Self::person).
    pub fn identity(&self) -> Result<Option<Identity>> {
        let Some(bot) = self.meta("bot_id")? else {
            return Ok(None);
        };
        let owner = self.meta("owner_id")?.filter(|o| !o.is_empty());
        Ok(Some(Identity { bot, owner }))
    }

    /// The identity, only when it names a person: without one we can't tell
    /// your own edits from others', so nothing is recorded.
    pub fn person(&self) -> Result<Option<Identity>> {
        Ok(self.identity()?.filter(|i| i.owner.is_some()))
    }

    fn since(&self) -> Result<DateTime<Utc>> {
        Ok(self
            .meta("activity_since")?
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map_or(DateTime::UNIX_EPOCH, |t| t.with_timezone(&Utc)))
    }

    /// Records the others' comments of `list` (just pulled for `parent_id`, a
    /// page or block) created after the inbox began; one that mentions you is
    /// a mention. Already known ones (read or not) stay as they are.
    pub fn note_comments(&self, parent_id: &str, list: &[Comment]) -> Result<()> {
        let Some(me) = self.person()? else {
            return Ok(());
        };
        let since = self.since()?;
        // the page must be cached (parent_id is it, or a block of it), else
        // the entry couldn't open anything
        let page = match self.block_page(parent_id)? {
            Some(p) => p,
            None if self.get_page(parent_id)?.is_some() => parent_id.to_string(),
            None => return Ok(()),
        };
        for c in list {
            if c.id.starts_with("local-") || me.is_me(&c.created_by) || c.created_time <= since {
                continue;
            }
            let mention = me.owner.as_deref().is_some_and(|o| {
                c.rich_text.iter().any(|r| {
                    matches!(&r.kind, RichTextKind::Mention(m) if m["type"] == "user" && m["user"]["id"] == o)
                })
            });
            let text = plain_text(&c.rich_text);
            self.conn.execute(
                "INSERT OR IGNORE INTO activity (kind, target, page_id, parent_id, author, summary, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    if mention { MENTION } else { COMMENT },
                    c.id,
                    page,
                    parent_id,
                    c.created_by,
                    text.chars().take(120).collect::<String>(),
                    ts(&c.created_time)
                ],
            )?;
        }
        Ok(())
    }

    /// Records `page` (just pulled) if someone else edited it after you last
    /// opened it here (and after the inbox began). A newer edit of an entry
    /// already there makes it unread again.
    pub fn note_page(&self, page: &Page) -> Result<()> {
        let (Some(me), Some(by)) = (self.person()?, page.last_edited_by()) else {
            return Ok(());
        };
        let opened = self
            .conn
            .query_row(
                "SELECT at FROM opened WHERE page_id = ?1",
                [&page.id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map_or(DateTime::UNIX_EPOCH, |t| t.with_timezone(&Utc));
        if me.is_me(by) || page.in_trash || page.last_edited_time <= self.since()?.max(opened) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO activity (kind, target, page_id, author, at) VALUES ('page', ?1, ?1, ?2, ?3)
             ON CONFLICT(kind, target) DO UPDATE SET author = excluded.author, at = excluded.at, unread = 1
             WHERE excluded.at > activity.at",
            params![page.id, by, ts(&page.last_edited_time)],
        )?;
        Ok(())
    }

    /// The page was opened (now): its entries are read, and edits from before
    /// now are no longer news.
    pub fn mark_page_read(&self, page_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO opened VALUES (?1, ?2)",
            params![page_id, ts(&Utc::now())],
        )?;
        self.conn.execute(
            "UPDATE activity SET unread = 0 WHERE page_id = ?1 AND unread = 1",
            [page_id],
        )?;
        Ok(())
    }

    pub fn mark_entry_read(&self, kind: &str, target: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE activity SET unread = 0 WHERE kind = ?1 AND target = ?2",
            [kind, target],
        )?;
        Ok(())
    }

    pub fn mark_all_read(&self) -> Result<()> {
        self.conn
            .execute("UPDATE activity SET unread = 0 WHERE unread = 1", [])?;
        Ok(())
    }

    pub fn unread_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM activity WHERE unread = 1", [], |r| {
                r.get::<_, i64>(0)
            })? as usize)
    }

    /// Newest first, read and unread.
    pub fn activity(&self) -> Result<Vec<Entry>> {
        let mut st = self.conn.prepare(
            "SELECT kind, target, page_id, parent_id, author, summary, at, unread FROM activity ORDER BY at DESC, target",
        )?;
        let rows = st.query_map([], |r| {
            let at: String = r.get(6)?;
            Ok(Entry {
                kind: r.get(0)?,
                target: r.get(1)?,
                page_id: r.get(2)?,
                parent_id: r.get(3)?,
                author: r.get(4)?,
                summary: r.get(5)?,
                at: DateTime::parse_from_rfc3339(&at)
                    .map_or(DateTime::UNIX_EPOCH, |t| t.with_timezone(&Utc)),
                unread: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const ME: &str = "owner";

    fn store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.set_identity("bot", None, Some(ME)).unwrap();
        s.set_meta("activity_since", "2020-01-01T00:00:00.000Z")
            .unwrap();
        for id in ["p", "q"] {
            let mut p = page("x", "2000-01-01T00:00:00.000Z");
            p.id = id.into();
            s.upsert_page(&p).unwrap();
        }
        s
    }

    fn comment(id: &str, by: &str, rich: Value) -> Comment {
        serde_json::from_value::<Comment>(json!({
            "object": "comment", "id": id, "discussion_id": "d", "parent": {"type": "page_id", "page_id": "p"},
            "created_time": "2021-01-01T00:00:00.000Z", "last_edited_time": "2021-01-01T00:00:00.000Z",
            "created_by": {"object": "user", "id": by}, "rich_text": rich,
        }))
        .unwrap()
    }

    fn text(t: &str) -> Value {
        json!([{"type": "text", "text": {"content": t}, "plain_text": t, "annotations": {}}])
    }

    fn page(by: &str, edited: &str) -> Page {
        serde_json::from_value::<Page>(json!({
            "object": "page", "id": "p", "parent": {"type": "workspace", "workspace": true},
            "created_time": "2021-01-01T00:00:00.000Z", "last_edited_time": edited, "in_trash": false,
            "url": "u", "properties": {}, "last_edited_by": {"object": "user", "id": by},
        }))
        .unwrap()
    }

    #[test]
    fn a_new_comment_is_one_unread_entry_however_often_it_is_pulled() {
        let s = store();
        let c = comment("c1", "u1", text("hello"));
        s.note_comments("p", std::slice::from_ref(&c)).unwrap();
        s.mark_all_read().unwrap();
        s.note_comments("p", &[c, comment("c2", "u1", text("again"))])
            .unwrap();
        let list = s.activity().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(s.unread_count().unwrap(), 1, "c1 stays read, c2 is new");
        let c1 = list.iter().find(|e| e.target == "c1").unwrap();
        assert_eq!(
            (c1.kind.as_str(), c1.page_id.as_str(), c1.summary.as_str()),
            (COMMENT, "p", "hello")
        );
    }

    #[test]
    fn a_mention_of_you_is_flagged_and_your_own_comments_are_not_recorded() {
        let s = store();
        let m = json!([{"type": "mention", "mention": {"type": "user", "user": {"id": ME}}, "plain_text": "@me", "annotations": {}}]);
        let other = json!([{"type": "mention", "mention": {"type": "user", "user": {"id": "x"}}, "plain_text": "@x", "annotations": {}}]);
        s.note_comments(
            "p",
            &[
                comment("m", "u1", m),
                comment("o", "u1", other),
                comment("mine", ME, text("by me")),
                comment("bot", "bot", text("by the app")),
                comment("local-1", "u1", text("unpushed")),
            ],
        )
        .unwrap();
        let kinds: Vec<_> = s
            .activity()
            .unwrap()
            .into_iter()
            .map(|e| (e.target, e.kind))
            .collect();
        assert_eq!(kinds.len(), 2, "{kinds:?}");
        assert!(
            kinds.contains(&("m".into(), MENTION.into()))
                && kinds.contains(&("o".into(), COMMENT.into()))
        );
    }

    #[test]
    fn without_a_person_owner_nothing_is_a_mention_and_without_identity_nothing_is_recorded() {
        let s = store();
        s.set_meta("bot_id", "").unwrap();
        s.set_meta("owner_id", "").unwrap();
        s.note_comments("p", &[comment("c", "u1", text("x"))])
            .unwrap();
        s.note_page(&page("u1", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        assert!(s.activity().unwrap().is_empty(), "no identity");
        s.set_identity("bot", None, None).unwrap();
        s.note_comments("p", &[comment("c", "u1", text("x"))])
            .unwrap();
        s.note_page(&page("u1", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        assert!(
            s.activity().unwrap().is_empty(),
            "workspace-owned and NOTION_USER_ID unset: no person"
        );
    }

    #[test]
    fn the_env_user_wins_over_the_api_owner_and_is_who_you_are() {
        let s = store();
        s.set_identity("bot", Some("api-owner"), Some("env-user"))
            .unwrap();
        assert_eq!(
            s.identity().unwrap().unwrap().owner.as_deref(),
            Some("env-user")
        );
        s.set_identity("bot", Some("api-owner"), None).unwrap();
        assert_eq!(
            s.identity().unwrap().unwrap().owner.as_deref(),
            Some("api-owner")
        );
        s.set_identity("bot", None, Some("env-user")).unwrap();
        s.note_comments("p", &[comment("mine", "env-user", text("me"))])
            .unwrap();
        s.note_page(&page("env-user", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        assert!(s.activity().unwrap().is_empty());
    }

    #[test]
    fn a_comment_on_an_uncached_page_or_block_is_skipped() {
        let s = store();
        s.note_comments("nowhere", &[comment("c", "u1", text("x"))])
            .unwrap();
        assert!(s.activity().unwrap().is_empty());
    }

    #[test]
    fn comments_from_before_the_inbox_began_are_not_news() {
        let s = store();
        s.set_meta("activity_since", "2022-01-01T00:00:00.000Z")
            .unwrap();
        s.note_comments("p", &[comment("c", "u1", text("old"))])
            .unwrap();
        assert!(s.activity().unwrap().is_empty());
    }

    #[test]
    fn pages_others_edited_after_you_opened_them_are_entries_yours_are_not() {
        let s = store();
        s.note_page(&page("bot", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        s.note_page(&page(ME, "2030-01-01T00:00:00.000Z")).unwrap();
        assert!(s.activity().unwrap().is_empty());
        s.note_page(&page("u1", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        s.note_page(&page("u1", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        assert_eq!(
            (s.activity().unwrap().len(), s.unread_count().unwrap()),
            (1, 1)
        );
        // opening reads it; the same edit isn't news again, a later one is
        s.mark_page_read("p").unwrap();
        assert_eq!(s.unread_count().unwrap(), 0);
        s.note_page(&page("u1", "2030-01-01T00:00:00.000Z"))
            .unwrap();
        assert_eq!(s.unread_count().unwrap(), 0);
        s.note_page(&page("u1", "2031-01-01T00:00:00.000Z"))
            .unwrap();
        assert_eq!(
            (s.activity().unwrap().len(), s.unread_count().unwrap()),
            (1, 1)
        );
        // an edit from before you opened it is not news
        let t = Store::open_in_memory().unwrap();
        t.set_identity("bot", None, Some(ME)).unwrap();
        t.mark_page_read("p").unwrap();
        t.note_page(&page("u1", "2020-01-01T00:00:00.000Z"))
            .unwrap();
        assert!(t.activity().unwrap().is_empty());
    }

    #[test]
    fn opening_a_page_marks_its_entries_read_and_mark_all_read_clears_the_count() {
        let s = store();
        let mut other = comment("c2", "u1", text("on q"));
        other.parent = crate::model::Parent::Page("q".into());
        s.note_comments("p", &[comment("c1", "u1", text("on p"))])
            .unwrap();
        s.note_comments("q", &[other]).unwrap();
        assert_eq!(s.unread_count().unwrap(), 2);
        s.mark_page_read("p").unwrap();
        assert_eq!(s.unread_count().unwrap(), 1);
        s.mark_entry_read(COMMENT, "c2").unwrap();
        assert_eq!(s.unread_count().unwrap(), 0);
        s.note_comments("p", &[comment("c3", "u1", text("n"))])
            .unwrap();
        s.mark_all_read().unwrap();
        assert_eq!(
            (s.unread_count().unwrap(), s.activity().unwrap().len()),
            (0, 3)
        );
    }

    #[test]
    fn a_v3_store_migrates_and_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE users (id TEXT PRIMARY KEY, type TEXT NOT NULL DEFAULT '', name TEXT NOT NULL DEFAULT '',
                   avatar_url TEXT, email TEXT, raw TEXT NOT NULL);
                 INSERT INTO users VALUES ('u', 'person', 'Ada', NULL, NULL, '{}');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.users().unwrap()[0].name, "Ada");
        assert_eq!(
            (s.unread_count().unwrap(), s.identity().unwrap()),
            (0, None)
        );
        assert!(s.since().unwrap() > DateTime::UNIX_EPOCH);
    }
}
