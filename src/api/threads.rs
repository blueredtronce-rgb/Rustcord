//! Thread and forum REST calls (FEATURES_NOTES §2.1, §2.2), answered through
//! `backend::handle_social`. Same rules as the other social calls: one request per user action,
//! nothing retried on 4xx (the single 202 "index not ready" retry of a thread search is part of
//! the user's own request), captcha answers become "do it in the official app" in `error_from`.
//! Discord's error messages for these routes (160004 thread exists, 160005-160007 locked / too
//! many threads, 50083 archived, 40058, 40061, 400001) are readable English and pass through.
//!
//! OWNERSHIP: threads-forums engineer (docs/SOCIAL_DESIGN.md). The method signatures are the
//! contract used by backend.rs.

use super::upload::{self, MAX_FILES};
use super::{ApiClient, ApiError, net_err};
use crate::backend::{ArchivedThreads, ForumPost, ThreadList, ThreadQuery, ThreadSort};
use crate::model::{Channel, Id, Message, ThreadMember, ThreadPage, channel_type, null_default};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// Thread search page size (the API maximum) and its highest offset.
const SEARCH_LIMIT: u32 = 25;
const MAX_OFFSET: u32 = 9_975;
/// Archived list page size (API: 2-100).
const ARCHIVED_LIMIT: u32 = 50;
/// Search limits: title length, tag filters.
const MAX_NAME: usize = 100;
const MAX_TAG_FILTERS: usize = 20;

impl ApiClient {
    /// One page of a thread list (§2.1, §2.2):
    /// - `Active { offset }`: `GET /channels/{c}/threads/search?archived=false&sort_by=last_message_time&sort_order=desc&limit=25&offset=`
    ///   (the per-channel "active threads" routes are not usable by users).
    /// - `Forum(q)`: same route with `name`, repeated `tag` + `tag_setting`, `archived` (omitted =
    ///   both), `sort_by=last_message_time|creation_time|relevance` -> `{threads, members,
    ///   has_more, total_results, first_messages?}`.
    /// - `Archived { Public | Private, before }`: `GET /channels/{c}/threads/archived/{public|private}?[before=<ISO>&]limit=50`;
    ///   `Joined`: `GET /channels/{c}/users/@me/threads/archived/private?[before=<id>&]limit=50`
    ///   -> `{threads, members, has_more}`.
    ///
    /// 202 (code 110000, index not ready): one delayed retry (`retry_after`, ≤ 10 s), then an
    /// error. `members[]` (mine) go into each thread's `Channel::member` (matched by `id`).
    pub async fn threads(&self, channel_id: Id, list: &ThreadList) -> Result<ThreadPage, ApiError> {
        let url = threads_url(channel_id, list);
        let mut retried = false;
        loop {
            let resp = self.send(Method::GET, url.clone(), None).await?;
            let indexing = resp.status().as_u16() == 202;
            let body = resp.bytes().await.map_err(net_err)?;
            if !indexing {
                return thread_page(&body);
            }
            if retried {
                let message = "Discord is still indexing this channel. Try again in a moment.".into();
                return Err(ApiError::Http { status: 202, message });
            }
            retried = true;
            tokio::time::sleep(Duration::from_secs_f32(super::chat::index_wait(&body))).await;
        }
    }

    /// `PUT /channels/{t}/thread-members/@me?location=Context+Menu` -> 204 (unarchived threads
    /// only). THREAD_MEMBERS_UPDATE / THREAD_CREATE carry the result.
    pub async fn join_thread(&self, thread_id: Id) -> Result<(), ApiError> {
        self.send(Method::PUT, my_membership_url(thread_id), None).await.map(drop)
    }

    /// `DELETE /channels/{t}/thread-members/@me?location=Context+Menu` -> 204.
    pub async fn leave_thread(&self, thread_id: Id) -> Result<(), ApiError> {
        self.send(Method::DELETE, my_membership_url(thread_id), None).await.map(drop)
    }

    /// `from_message` Some: `POST /channels/{c}/messages/{m}/threads {"name","auto_archive_duration","location":"Message"}`
    /// (the server picks the type from the parent; thread id = message id);
    /// None: `POST /channels/{c}/threads {"name","auto_archive_duration","type":10|11|12,"location":"Plus Button"}`
    /// (`type` always sent: the server defaults to private; 10 under an announcement
    /// `parent_kind`). -> the thread channel.
    pub async fn create_thread(
        &self,
        channel_id: Id,
        name: &str,
        from_message: Option<Id>,
        private: bool,
        parent_kind: u8,
        auto_archive_minutes: u32,
    ) -> Result<Channel, ApiError> {
        let (url, body) = create_thread_request(channel_id, name, from_message, private, parent_kind, auto_archive_minutes);
        self.send_json(Method::POST, url, Some(body)).await
    }

    /// `PATCH /channels/{t} {"archived": bool}` -> the channel (dropped; THREAD_UPDATE follows).
    pub async fn set_thread_archived(&self, thread_id: Id, archived: bool) -> Result<(), ApiError> {
        let url = Self::url(&["channels", &thread_id.to_string()]);
        self.send(Method::PATCH, url, Some(json!({ "archived": archived }))).await.map(drop)
    }
}

/// New forum/media post (§2.2): steps 1-2 of `upload::send_with_files` against the **forum**
/// id, then `POST /channels/{forum}/threads?use_nested_fields=true {"name","applied_tags":[…],
/// "message":{"content","attachments":[{"id","filename","uploaded_filename"}…]}}` -> the post
/// channel. Uploads are deleted again if the post fails. At least content or one file.
pub async fn create_forum_post(api: &ApiClient, channel_id: Id, post: ForumPost) -> Result<Channel, ApiError> {
    if post.content.trim().is_empty() && post.files.is_empty() {
        return Err(local("Write something or attach a file."));
    }
    if post.files.len() > MAX_FILES {
        return Err(local(&format!("A post can have at most {MAX_FILES} files.")));
    }
    let mut started = Vec::new();
    let uploaded = if post.files.is_empty() {
        Vec::new()
    } else {
        match upload::upload_all(api, channel_id, &post.files, &|_, _| {}, &mut started).await {
            Ok(names) => names,
            Err(e) => {
                upload::delete_uploads(api, &started).await;
                return Err(e);
            }
        }
    };
    let mut url = ApiClient::url(&["channels", &channel_id.to_string(), "threads"]);
    url.query_pairs_mut().append_pair("use_nested_fields", "true");
    let body = post_body(&post, &uploaded);
    match api.send_json(Method::POST, url, Some(body)).await {
        Ok(c) => Ok(c),
        Err(e) => {
            upload::delete_uploads(api, &started).await;
            Err(upload::readable(e))
        }
    }
}

fn local(message: &str) -> ApiError {
    ApiError::Http { status: 0, message: message.into() }
}

fn my_membership_url(thread_id: Id) -> Url {
    let mut url = ApiClient::url(&["channels", &thread_id.to_string(), "thread-members", "@me"]);
    url.query_pairs_mut().append_pair("location", "Context Menu");
    url
}

fn threads_url(channel_id: Id, list: &ThreadList) -> Url {
    let c = channel_id.to_string();
    let (kind, before) = match list {
        ThreadList::Active { offset } => {
            return search_url(&c, &ThreadQuery { archived: Some(false), offset: *offset, ..Default::default() });
        }
        ThreadList::Forum(q) => return search_url(&c, q),
        ThreadList::Archived { kind, before } => (kind, before),
    };
    let mut url = match kind {
        ArchivedThreads::Public => ApiClient::url(&["channels", &c, "threads", "archived", "public"]),
        ArchivedThreads::Private => ApiClient::url(&["channels", &c, "threads", "archived", "private"]),
        ArchivedThreads::Joined => ApiClient::url(&["channels", &c, "users", "@me", "threads", "archived", "private"]),
    };
    {
        let mut p = url.query_pairs_mut();
        if let Some(b) = before {
            p.append_pair("before", b);
        }
        p.append_pair("limit", &ARCHIVED_LIMIT.to_string());
    }
    url
}

/// `GET /channels/{c}/threads/search` (§2.2 parameter table).
fn search_url(channel: &str, q: &ThreadQuery) -> Url {
    let mut url = ApiClient::url(&["channels", channel, "threads", "search"]);
    let name = q.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let sort = match q.sort {
        ThreadSort::Created => "creation_time",
        // Relevance needs a title search; without one the API default is the sensible order.
        ThreadSort::Relevance if name.is_some() => "relevance",
        ThreadSort::LastActivity | ThreadSort::Relevance => "last_message_time",
    };
    {
        let mut p = url.query_pairs_mut();
        if let Some(n) = name {
            p.append_pair("name", &n.chars().take(MAX_NAME).collect::<String>());
        }
        for t in q.tags.iter().take(MAX_TAG_FILTERS) {
            p.append_pair("tag", &t.to_string());
        }
        if !q.tags.is_empty() {
            p.append_pair("tag_setting", if q.match_all_tags { "match_all" } else { "match_some" });
        }
        if let Some(a) = q.archived {
            p.append_pair("archived", if a { "true" } else { "false" });
        }
        p.append_pair("sort_by", sort)
            .append_pair("sort_order", "desc")
            .append_pair("limit", &SEARCH_LIMIT.to_string())
            .append_pair("offset", &q.offset.min(MAX_OFFSET).to_string());
    }
    url
}

/// Thread search and archived list answers.
#[derive(Deserialize, Default)]
#[serde(default)]
struct PageWire {
    #[serde(deserialize_with = "null_default")]
    threads: Vec<Channel>,
    #[serde(deserialize_with = "null_default")]
    members: Vec<ThreadMember>,
    #[serde(deserialize_with = "null_default")]
    has_more: bool,
    total_results: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    first_messages: Vec<Message>,
}

fn thread_page(body: &[u8]) -> Result<ThreadPage, ApiError> {
    let w: PageWire = serde_json::from_slice(body).map_err(|e| ApiError::Decode(e.to_string()))?;
    let mut members = w.members;
    let threads = (w.threads.into_iter())
        .map(|mut t| {
            if let Some(i) = members.iter().position(|m| m.id == Some(t.id)) {
                t.member = Some(members.swap_remove(i));
            }
            t
        })
        .collect();
    Ok(ThreadPage { threads, has_more: w.has_more, total_results: w.total_results, first_messages: w.first_messages })
}

fn create_thread_request(channel_id: Id, name: &str, from_message: Option<Id>, private: bool, parent_kind: u8, minutes: u32) -> (Url, Value) {
    let c = channel_id.to_string();
    let mut body = json!({ "name": name.trim(), "auto_archive_duration": minutes });
    match from_message {
        Some(m) => {
            body["location"] = json!("Message");
            (ApiClient::url(&["channels", &c, "messages", &m.to_string(), "threads"]), body)
        }
        None => {
            body["type"] = json!(match (parent_kind, private) {
                (channel_type::GUILD_ANNOUNCEMENT, _) => channel_type::ANNOUNCEMENT_THREAD,
                (_, true) => channel_type::PRIVATE_THREAD,
                (_, false) => channel_type::PUBLIC_THREAD,
            });
            body["location"] = json!("Plus Button");
            (ApiClient::url(&["channels", &c, "threads"]), body)
        }
    }
}

/// Body of a new forum post; `uploaded` = the files' `upload_filename`s, in order.
fn post_body(post: &ForumPost, uploaded: &[String]) -> Value {
    let mut message = json!({ "content": post.content });
    if !uploaded.is_empty() {
        message["attachments"] = upload::attachments_json(&post.files, uploaded);
    }
    json!({ "name": post.name.trim(), "auto_archive_duration": post.auto_archive_minutes, "applied_tags": post.applied_tags, "message": message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{UploadFile, UploadSource};

    fn file() -> UploadFile {
        UploadFile { source: UploadSource::Bytes(std::sync::Arc::from(&b"a"[..])), filename: "a.png".into(), spoiler: false, description: None }
    }

    fn query(list: &ThreadList) -> String {
        let url = threads_url(Id(5), list);
        format!("{}?{}", url.path(), url.query().unwrap_or(""))
    }

    #[test]
    fn list_urls() {
        assert_eq!(
            query(&ThreadList::Active { offset: 25 }),
            "/api/v9/channels/5/threads/search?archived=false&sort_by=last_message_time&sort_order=desc&limit=25&offset=25"
        );
        let q = ThreadQuery {
            name: Some("  bug report ".into()),
            tags: vec![Id(7), Id(8)],
            match_all_tags: true,
            archived: None,
            sort: ThreadSort::Relevance,
            offset: 99_999,
        };
        assert_eq!(
            query(&ThreadList::Forum(q.clone())),
            "/api/v9/channels/5/threads/search?name=bug+report&tag=7&tag=8&tag_setting=match_all&sort_by=relevance&sort_order=desc&limit=25&offset=9975"
        );
        let no_name = ThreadQuery { name: Some(" ".into()), tags: vec![], ..q };
        assert!(query(&ThreadList::Forum(no_name)).ends_with("search?sort_by=last_message_time&sort_order=desc&limit=25&offset=9975"));
        let created = ThreadQuery { sort: ThreadSort::Created, archived: Some(true), ..Default::default() };
        assert!(query(&ThreadList::Forum(created)).ends_with("search?archived=true&sort_by=creation_time&sort_order=desc&limit=25&offset=0"));

        let archived = |kind, before: Option<&str>| query(&ThreadList::Archived { kind, before: before.map(Into::into) });
        assert_eq!(archived(ArchivedThreads::Public, None), "/api/v9/channels/5/threads/archived/public?limit=50");
        assert_eq!(
            archived(ArchivedThreads::Private, Some("2026-01-02T03:04:05.000000+00:00")),
            "/api/v9/channels/5/threads/archived/private?before=2026-01-02T03%3A04%3A05.000000%2B00%3A00&limit=50"
        );
        assert_eq!(
            archived(ArchivedThreads::Joined, Some("123")),
            "/api/v9/channels/5/users/@me/threads/archived/private?before=123&limit=50"
        );
        assert_eq!(my_membership_url(Id(9)).as_str(), "https://discord.com/api/v9/channels/9/thread-members/@me?location=Context+Menu");
    }

    #[test]
    fn search_page_fixture() {
        // Shape of a forum thread search answer (UD threads.mdx), trimmed.
        let body = br#"{
          "threads": [
            {"id":"300","type":11,"guild_id":"10","parent_id":"101","owner_id":"2","name":"Post A",
             "last_message_id":"310","message_count":4,"member_count":3,"applied_tags":["7"],"flags":2,
             "thread_metadata":{"archived":false,"auto_archive_duration":4320,"archive_timestamp":"2026-11-01T00:00:00+00:00","locked":false,"create_timestamp":"2026-11-01T00:00:00+00:00"}},
            {"id":"301","type":11,"guild_id":"10","parent_id":"101","name":"Post B","applied_tags":null,
             "thread_metadata":{"archived":true,"auto_archive_duration":60,"archive_timestamp":"2026-10-01T00:00:00+00:00","locked":true}}
          ],
          "members": [{"id":"300","user_id":"1","join_timestamp":"2026-11-01T00:00:00+00:00","flags":1}],
          "has_more": true,
          "total_results": 42,
          "first_messages": [{"id":"300","channel_id":"300","content":"hello","author":{"id":"2","username":"zed"}}]
        }"#;
        let p = thread_page(body).unwrap();
        assert_eq!((p.has_more, p.total_results, p.threads.len()), (true, Some(42), 2));
        let a = &p.threads[0];
        assert_eq!(a.member.as_ref().map(|m| m.flags), Some(1), "my membership attached");
        assert_eq!((a.applied_tags.as_slice(), a.message_count, a.flags), (&[Id(7)][..], Some(4), 2));
        assert!(p.threads[1].member.is_none() && p.threads[1].is_archived());
        assert!(p.threads[1].thread_metadata.as_ref().unwrap().locked);
        assert_eq!(p.first_messages[0].content, "hello");

        // Archived lists: no total, no first messages.
        let p = thread_page(br#"{"threads":[],"members":[],"has_more":false}"#).unwrap();
        assert_eq!((p.has_more, p.total_results, p.first_messages.len()), (false, None, 0));
        assert!(matches!(thread_page(b"<html>"), Err(ApiError::Decode(_))));
    }

    #[test]
    fn create_thread_bodies() {
        let text = channel_type::GUILD_TEXT;
        let (url, body) = create_thread_request(Id(5), " plans ", None, true, text, 1440);
        assert_eq!(url.path(), "/api/v9/channels/5/threads");
        assert_eq!(body, json!({"name":"plans","auto_archive_duration":1440,"type":12,"location":"Plus Button"}));
        let (_, body) = create_thread_request(Id(5), "x", None, false, text, 4320);
        assert_eq!(body["type"], 11, "public must be explicit");
        let (_, body) = create_thread_request(Id(5), "news", None, false, channel_type::GUILD_ANNOUNCEMENT, 4320);
        assert_eq!(body["type"], 10, "announcement parent");
        let (url, body) = create_thread_request(Id(5), "re: that", Some(Id(77)), false, channel_type::GUILD_ANNOUNCEMENT, 60);
        assert_eq!(url.path(), "/api/v9/channels/5/messages/77/threads");
        assert_eq!(body, json!({"name":"re: that","auto_archive_duration":60,"location":"Message"}));
    }

    #[test]
    fn forum_post_body() {
        let post = ForumPost { name: " Title ".into(), content: "body".into(), applied_tags: vec![Id(7)], files: vec![file()], auto_archive_minutes: 4320 };
        let expected = json!({"name":"Title","auto_archive_duration":4320,"applied_tags":["7"],"message":{"content":"body"}});
        assert_eq!(post_body(&ForumPost { files: vec![], ..post.clone() }, &[]), expected);
        let b = post_body(&post, &["abc/a.png".into()]);
        assert_eq!(b["message"]["attachments"], json!([{"id":"0","filename":"a.png","uploaded_filename":"abc/a.png"}]));
    }

    #[tokio::test]
    async fn empty_or_oversized_posts_fail_before_any_request() {
        // Checked locally: no request goes out.
        let props = std::sync::Arc::new(crate::api::props::ClientProps::new(&crate::config::Settings::default()));
        let api = ApiClient::new(crate::auth::Token::parse(&"t".repeat(30)).unwrap(), props);
        let empty = ForumPost { name: "t".into(), content: "  ".into(), ..Default::default() };
        assert_eq!(create_forum_post(&api, Id(5), empty).await.unwrap_err().to_string(), "Write something or attach a file.");
        let many = ForumPost { name: "t".into(), files: vec![file(); MAX_FILES + 1], ..Default::default() };
        assert!(create_forum_post(&api, Id(5), many).await.unwrap_err().to_string().contains("at most"));
    }
}
