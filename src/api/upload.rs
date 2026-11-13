//! Attachments via cloud upload (FEATURES_NOTES §1.1; reqwest has no `multipart` here).
//!
//! 1. `POST /channels/{c}/attachments {"files":[{"id":"0","filename","file_size"}]}` (API host,
//!    token) -> `upload_url` + `upload_filename` per echoed `id`.
//! 2. `PUT {upload_url}` with the raw bytes from a client that has **no** `Authorization`, no
//!    `Content-Type` and no Discord headers (never `ApiClient::http`). Retry only
//!    500/502/504/507/522/523/524, at most 5 times, sleeping `1 + 2*try` s.
//! 3. `POST /channels/{c}/messages` like `send_body`, plus `attachments: [{id, filename,
//!    uploaded_filename, description?}]` (`SPOILER_` prefix for spoilers).
//!
//! Cleanup: `DELETE /attachments/{upload_filename}` (the `/` percent-encoded as `%2F`, which
//! `ApiClient::url` does per segment) for every started upload when the user cancels or a later
//! step fails; a retry uploads again.
//!
//! OWNERSHIP: upload engineer (docs/CHAT_ESSENTIALS_DESIGN.md). The signature is the contract.

use super::{ApiClient, ApiError, net_err, send_body};
use crate::backend::{ReplyTo, UploadFile, UploadSource};
use crate::model::{Id, Message};
use reqwest::{Method, Url};
use serde_json::{Value, json};
use std::io::Read;
use std::time::Duration;
use tokio::sync::oneshot;

/// Files per message.
pub const MAX_FILES: usize = 10;
/// Largest file the cloud PUT accepts (UD); the per-account limit is the UI's pre-check
/// (`AppState::upload_limit`) and the server's.
const MAX_FILE_SIZE: u64 = 500 * 1024 * 1024;
/// The only host an `upload_url` may point at (FEATURES_NOTES §1.1); the bytes go nowhere else.
const UPLOAD_HOST: &str = "discord-attachments-uploads-prd.storage.googleapis.com";
/// Storage answers worth another PUT (DPS).
const RETRY_STATUSES: [u16; 7] = [500, 502, 504, 507, 522, 523, 524];
const MAX_PUT_RETRIES: u64 = 5;
/// Slowest upload speed tolerated before a PUT times out (bytes/s), on top of a fixed minute.
const MIN_PUT_SPEED: u64 = 64 * 1024;
/// One upload at a time process-wide, so at most one file (≤ MAX_FILE_SIZE) is in memory even
/// when several sends with attachments overlap; later sends queue (FIFO).
static UPLOAD_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Uploads `files` and sends the message (steps above).
/// - `progress(sent, total)`: bytes of finished files out of all files, after each PUT.
/// - `cancel` resolving (a send or a dropped sender) during steps 1-2 stops at once: started
///   uploads are deleted and `Ok(None)` is returned (no `SendResult`). Step 3 is not cancelled
///   once it is on the wire, so a message is either sent whole or not at all.
/// - `Path` sources are read with `spawn_blocking` (tokio has no `fs` feature here).
// shortcut: one whole file in memory at a time across all sends (`UPLOAD_SLOT`, ≤ MAX_FILE_SIZE) and per-file progress;
// streaming from disk and byte-level progress need reqwest's `stream` feature (not approved).
#[allow(clippy::too_many_arguments)]
pub async fn send_with_files(
    api: &ApiClient,
    channel_id: Id,
    content: &str,
    nonce: &str,
    reply: Option<&ReplyTo>,
    files: Vec<UploadFile>,
    progress: impl Fn(u64, u64) + Send + Sync,
    cancel: oneshot::Receiver<()>,
) -> Result<Option<Message>, ApiError> {
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(local(format!("You can send 1 to {MAX_FILES} files per message.")));
    }
    let mut started = Vec::new();
    let uploaded = {
        let work = upload_all(api, channel_id, &files, &progress, &mut started);
        tokio::select! {
            r = work => Some(r),
            _ = cancel => None,
        }
    };
    let slots = match uploaded {
        Some(Ok(slots)) => slots,
        Some(Err(e)) => {
            delete_uploads(api, &started).await;
            return Err(e);
        }
        None => {
            delete_uploads(api, &started).await;
            return Ok(None);
        }
    };
    let mut body = send_body(channel_id, content, nonce, reply);
    body["attachments"] = attachments_json(&files, &slots);
    let url = ApiClient::url(&["channels", &channel_id.to_string(), "messages"]);
    match api.send_json(Method::POST, url, Some(body)).await {
        Ok(m) => Ok(Some(m)),
        Err(e) => {
            delete_uploads(api, &started).await;
            Err(readable(e))
        }
    }
}

/// Steps 1 and 2; returns the `upload_filename`s in file order. `started` collects every
/// upload a PUT was begun for, so the caller can delete them on cancel or failure.
pub(super) async fn upload_all(
    api: &ApiClient,
    channel_id: Id,
    files: &[UploadFile],
    progress: &(impl Fn(u64, u64) + Send + Sync),
    started: &mut Vec<String>,
) -> Result<Vec<String>, ApiError> {
    // Taken before step 1, not per file, so a queued send's upload URLs are fresh when used.
    // Released on return, or when the caller's cancel branch drops this future.
    let _slot = UPLOAD_SLOT.acquire().await.map_err(|_| local("Upload stopped.".into()))?;
    let sizes = blocking(file_sizes(files.to_vec())).await??;
    check_sizes(files, &sizes)?;
    let url = ApiClient::url(&["channels", &channel_id.to_string(), "attachments"]);
    let answer: Value = api.send_json(Method::POST, url, Some(slots_request(files, &sizes))).await.map_err(readable)?;
    let slots = parse_slots(&answer, files.len())?;
    let http = storage_client()?;
    let total: u64 = sizes.iter().sum();
    let mut sent = 0;
    for ((file, size), (url, name)) in files.iter().zip(&sizes).zip(&slots) {
        let bytes = blocking(read_source(file.source.clone(), *size)).await??;
        started.push(name.clone());
        put_file(&http, url.clone(), bytes, &file.filename).await?;
        sent += size;
        progress(sent, total);
    }
    Ok(slots.into_iter().map(|(_, name)| name).collect())
}

/// Best effort: a failed DELETE leaves an orphan Discord expires (or not) on its own.
pub(super) async fn delete_uploads(api: &ApiClient, names: &[String]) {
    for name in names {
        let _ = api.send(Method::DELETE, ApiClient::url(&["attachments", name]), None).await;
    }
}

fn local(message: String) -> ApiError {
    ApiError::Http { status: 0, message }
}

/// Runs blocking file I/O off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| local("Reading the file failed.".into()))
}

fn file_sizes(files: Vec<UploadFile>) -> impl FnOnce() -> Result<Vec<u64>, ApiError> {
    move || {
        files
            .iter()
            .map(|f| match &f.source {
                UploadSource::Bytes(b) => Ok(b.len() as u64),
                UploadSource::Path(p) => std::fs::metadata(p)
                    .map(|m| m.len())
                    .map_err(|e| local(format!("Could not read {}: {e}", f.filename))),
            })
            .collect()
    }
}

/// The file's bytes, reading at most `size + 1` so a file that grew cannot fill memory.
fn read_source(source: UploadSource, size: u64) -> impl FnOnce() -> Result<Vec<u8>, ApiError> {
    move || {
        let bytes = match source {
            UploadSource::Bytes(b) => b.to_vec(),
            UploadSource::Path(p) => {
                let mut buf = Vec::with_capacity(size as usize);
                std::fs::File::open(&p)
                    .and_then(|f| f.take(size + 1).read_to_end(&mut buf))
                    .map_err(|e| local(format!("Could not read {}: {e}", p.display())))?;
                buf
            }
        };
        if bytes.len() as u64 != size {
            return Err(local("A file changed while it was being sent. Try again.".into()));
        }
        Ok(bytes)
    }
}

fn check_sizes(files: &[UploadFile], sizes: &[u64]) -> Result<(), ApiError> {
    match files.iter().zip(sizes).find(|(_, s)| **s > MAX_FILE_SIZE) {
        Some((f, _)) => Err(local(format!("{} is larger than 500 MB, the most Discord accepts.", f.filename))),
        None => Ok(()),
    }
}

/// Name sent to Discord: spoilers get the `SPOILER_` prefix (once).
fn wire_name(f: &UploadFile) -> String {
    if f.spoiler && !f.filename.starts_with("SPOILER_") { format!("SPOILER_{}", f.filename) } else { f.filename.clone() }
}

fn slots_request(files: &[UploadFile], sizes: &[u64]) -> Value {
    let entries: Vec<Value> = files
        .iter()
        .zip(sizes)
        .enumerate()
        .map(|(i, (f, size))| json!({ "id": i.to_string(), "filename": wire_name(f), "file_size": size }))
        .collect();
    json!({ "files": entries })
}

/// Step 1's answer in file order (matched on the echoed `id`), each URL checked against
/// `UPLOAD_HOST` before anything is uploaded.
fn parse_slots(answer: &Value, n: usize) -> Result<Vec<(Url, String)>, ApiError> {
    let bad = || ApiError::Decode("upload slots".into());
    let mut slots = vec![None; n];
    for a in answer.get("attachments").and_then(Value::as_array).ok_or_else(bad)? {
        let id = match &a["id"] {
            Value::String(s) => s.parse::<usize>().ok(),
            v => v.as_u64().map(|i| i as usize),
        };
        let slot = id.and_then(|i| slots.get_mut(i)).ok_or_else(bad)?;
        let url = a["upload_url"].as_str().ok_or_else(bad)?;
        let name = a["upload_filename"].as_str().filter(|s| !s.is_empty()).ok_or_else(bad)?;
        *slot = Some((storage_url(url)?, name.to_owned()));
    }
    slots.into_iter().collect::<Option<Vec<_>>>().ok_or_else(bad)
}

fn storage_url(raw: &str) -> Result<Url, ApiError> {
    Url::parse(raw)
        .ok()
        .filter(|u| {
            u.scheme() == "https" && u.host_str() == Some(UPLOAD_HOST) && u.port().is_none() && u.username().is_empty() && u.password().is_none()
        })
        .ok_or_else(|| local("Discord sent an unexpected upload address, so nothing was uploaded.".into()))
}

pub(super) fn attachments_json(files: &[UploadFile], uploaded: &[String]) -> Value {
    files
        .iter()
        .zip(uploaded)
        .enumerate()
        .map(|(i, (f, up))| {
            let mut a = json!({ "id": i.to_string(), "filename": wire_name(f), "uploaded_filename": up });
            if let Some(d) = f.description.as_deref().filter(|d| !d.trim().is_empty()) {
                a["description"] = json!(d);
            }
            a
        })
        .collect()
}

/// Plain client for the storage host: no default headers, no token, no redirects (a redirect
/// must not carry the file anywhere else).
fn storage_client() -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(net_err)
}

/// Step 2. The body is reusable bytes, so a retry clones only a reference count.
async fn put_file(http: &reqwest::Client, url: Url, bytes: Vec<u8>, name: &str) -> Result<(), ApiError> {
    let timeout = Duration::from_secs(60 + bytes.len() as u64 / MIN_PUT_SPEED);
    let req = http.put(url).timeout(timeout).body(bytes);
    let mut attempt = 0;
    loop {
        let resp = req.try_clone().expect("byte bodies are cloneable").send().await.map_err(net_err)?;
        let status = resp.status().as_u16();
        if resp.status().is_success() {
            return Ok(());
        }
        if attempt >= MAX_PUT_RETRIES || !RETRY_STATUSES.contains(&status) {
            return Err(ApiError::Http { status, message: format!("Uploading {name} failed (HTTP {status}).") });
        }
        tokio::time::sleep(Duration::from_secs(1 + 2 * attempt)).await;
        attempt += 1;
    }
}

/// Cloudflare's 413 has an HTML body; Discord's own upload errors (40005, 50045, 50046,
/// 50146, 400001) already carry readable `message`s.
pub(super) fn readable(e: ApiError) -> ApiError {
    match e {
        ApiError::Http { status: 413, .. } => {
            ApiError::Http { status: 413, message: "The files are too large to send together.".into() }
        }
        e => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    fn file(name: &str, spoiler: bool, description: Option<&str>) -> UploadFile {
        UploadFile {
            source: UploadSource::Bytes(Arc::from(&b"abc"[..])),
            filename: name.into(),
            spoiler,
            description: description.map(Into::into),
        }
    }

    #[test]
    fn request_bodies_have_spoiler_prefix_and_alt_text() {
        let files = [file("cat.png", true, Some("a cat")), file("SPOILER_x.txt", true, Some(" ")), file("a.txt", false, None)];
        let req = slots_request(&files, &[3, 4, 5]);
        assert_eq!(
            req,
            json!({"files":[
                {"id":"0","filename":"SPOILER_cat.png","file_size":3},
                {"id":"1","filename":"SPOILER_x.txt","file_size":4},
                {"id":"2","filename":"a.txt","file_size":5}]})
        );
        let att = attachments_json(&files, &["u/0".into(), "u/1".into(), "u/2".into()]);
        assert_eq!(att[0], json!({"id":"0","filename":"SPOILER_cat.png","uploaded_filename":"u/0","description":"a cat"}));
        assert!(att[1].get("description").is_none(), "blank alt text is left out");
        assert_eq!(att[2], json!({"id":"2","filename":"a.txt","uploaded_filename":"u/2"}));
    }

    #[test]
    fn slots_match_ids_and_only_the_storage_host() {
        let url = |n: &str| format!("https://{UPLOAD_HOST}/uuid/{n}?upload_id=x");
        let answer = json!({"attachments":[
            {"id":"1","upload_url":url("b"),"upload_filename":"uuid/b"},
            {"id":0,"upload_url":url("a"),"upload_filename":"uuid/a"}]});
        let slots = parse_slots(&answer, 2).unwrap();
        assert_eq!(slots[0].1, "uuid/a");
        assert_eq!(slots[1].0.as_str(), url("b"));
        assert!(parse_slots(&answer, 3).is_err(), "a missing slot");
        assert!(parse_slots(&json!({"attachments":[{"id":"5","upload_url":url("a"),"upload_filename":"a"}]}), 1).is_err());

        for evil in [
            "http://discord-attachments-uploads-prd.storage.googleapis.com/a",
            "https://evil.example/a",
            "https://discord-attachments-uploads-prd.storage.googleapis.com.evil.example/a",
            "https://user@discord-attachments-uploads-prd.storage.googleapis.com/a",
            "https://discord-attachments-uploads-prd.storage.googleapis.com:8443/a",
            "https://discord.com/api/v9/a",
        ] {
            let answer = json!({"attachments":[{"id":"0","upload_url":evil,"upload_filename":"a"}]});
            assert!(parse_slots(&answer, 1).is_err(), "{evil}");
        }
    }

    #[test]
    fn limits_and_reading() {
        let files = [file("big.bin", false, None)];
        assert!(check_sizes(&files, &[MAX_FILE_SIZE]).is_ok());
        assert!(check_sizes(&files, &[MAX_FILE_SIZE + 1]).unwrap_err().to_string().contains("big.bin"));

        let path = std::env::temp_dir().join(format!("rustcord-upload-test-{}.txt", std::process::id()));
        std::fs::File::create(&path).unwrap().write_all(b"hello").unwrap();
        let on_disk = UploadFile { source: UploadSource::Path(path.clone()), ..file("hello.txt", false, None) };
        assert_eq!(file_sizes(vec![on_disk.clone(), files[0].clone()])().unwrap(), [5, 3]);
        assert_eq!(read_source(on_disk.source.clone(), 5)().unwrap(), b"hello");
        assert!(read_source(on_disk.source, 4)().is_err(), "grown file");
        std::fs::remove_file(&path).unwrap();
        assert!(file_sizes(vec![UploadFile { source: UploadSource::Path(path), ..file("gone.txt", false, None) }])().is_err());
    }

    #[test]
    fn delete_url_encodes_the_slash_and_413_reads_well() {
        let url = ApiClient::url(&["attachments", "a1b2-uuid/cat 1.png"]);
        assert_eq!(url.as_str(), "https://discord.com/api/v9/attachments/a1b2-uuid%2Fcat%201.png");
        assert!(readable(ApiError::Http { status: 413, message: String::new() }).to_string().contains("too large"));
    }

    /// No network: the count check and an early cancel both end before step 1.
    #[tokio::test]
    async fn bad_counts_fail_and_cancel_returns_none() {
        let props = Arc::new(super::super::props::ClientProps::new(&crate::config::Settings::default()));
        let api = ApiClient::new(crate::auth::Token::parse(&"t".repeat(30)).unwrap(), props);
        for n in [0, MAX_FILES + 1] {
            let files = vec![file("a", false, None); n];
            let (_tx, cancel) = oneshot::channel();
            let r = send_with_files(&api, Id(1), "", "1", None, files, |_, _| {}, cancel).await;
            assert!(matches!(r, Err(ApiError::Http { status: 0, .. })), "{n}: {r:?}");
        }
        let (tx, cancel) = oneshot::channel();
        tx.send(()).unwrap();
        let r = send_with_files(&api, Id(1), "", "1", None, vec![file("a", false, None)], |_, _| {}, cancel).await;
        assert!(matches!(r, Ok(None)), "{r:?}");
        let (tx, cancel) = oneshot::channel::<()>();
        drop(tx);
        let r = send_with_files(&api, Id(1), "", "1", None, vec![file("a", false, None)], |_, _| {}, cancel).await;
        assert!(matches!(r, Ok(None)), "a dropped sender cancels too: {r:?}");
    }

    /// The PUT reaches the storage host with the bytes and nothing that identifies the account.
    #[tokio::test]
    async fn put_carries_no_auth_or_discord_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let (mut req, mut buf) = (Vec::new(), [0u8; 4096]);
            while !req.ends_with(b"hello") {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "connection closed early");
                req.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").unwrap();
            String::from_utf8(req).unwrap().to_ascii_lowercase()
        });
        let url = Url::parse(&format!("http://{addr}/uuid/a.txt?upload_id=1")).unwrap();
        put_file(&storage_client().unwrap(), url, b"hello".to_vec(), "a.txt").await.unwrap();
        let req = server.join().unwrap();
        assert!(req.starts_with("put /uuid/a.txt?upload_id=1 "), "{req}");
        assert!(req.contains("content-length: 5"), "{req}");
        for h in ["authorization", "content-type", "x-super-properties", "cookie"] {
            assert!(!req.contains(h), "{h} in {req}");
        }
    }
}
