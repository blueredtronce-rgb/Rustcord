//! Opt-in local listening activity from PulseSync IPC or the Yandex Music Windows media session.
//! Reads only listening metadata. It never authenticates or handles account credentials.

use crate::activity::ActivityEvent;
use crate::backend::{BackendEvent, EventSink};
use crate::model::{Activity, Id, RichPresence};
use serde_json::{Value, json};
use std::sync::Arc;
use std::{io, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::watch;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as MediaSession,
    GlobalSystemMediaTransportControlsSessionManager as MediaSessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::DataReader;

const PIPE: &str = r"\\.\pipe\discord-ipc-0";
pub(crate) const PULSESYNC_ID: &str = "1124055337234858005";
const PULSESYNC_APP: Id = Id(1124055337234858005);

pub(crate) fn cover_url(url: &str) -> bool {
    url.len() <= 256 && reqwest::Url::parse(url).is_ok_and(|url| cover_host(&url))
}

pub(crate) fn cover_host(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("avatars.yandex.net" | "avatars.mds.yandex.net")
        )
        && (url.path().starts_with("/get-music-content/")
            || url.path().starts_with("/get-music-user-playlist/"))
}
const MAX_BODY: usize = 16 * 1024;
const HANDSHAKE: u32 = 0;
const FRAME: u32 = 1;
const CLOSE: u32 = 2;
const PING: u32 = 3;
const PONG: u32 = 4;
const MEDIA_REFRESH: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct Service(Option<tokio::task::JoinHandle<()>>);

impl Service {
    pub(crate) fn set_enabled(&mut self, enabled: bool, sink: &EventSink) {
        match (enabled, self.0.is_some()) {
            (true, false) => self.0 = Some(tokio::spawn(serve(sink.clone()))),
            (false, true) => {
                if let Some(task) = self.0.take() {
                    task.abort();
                }
                sink.send(BackendEvent::Activity(ActivityEvent::PulseSync(None)));
            }
            _ => {}
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn serve(sink: EventSink) {
    let (write, read) = watch::channel(None);
    tokio::join!(serve_pipe(write), display_activity(sink, read));
}

async fn serve_pipe(activity: watch::Sender<Option<Activity>>) {
    loop {
        // The first instance flag keeps this bridge from joining or taking over Discord's pipe.
        if let Ok(mut pipe) = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create(PIPE)
        {
            while pipe.connect().await.is_ok() {
                let _ = session(&mut pipe, |next| {
                    activity.send_replace(next);
                })
                .await;
                activity.send_if_modified(|current| current.take().is_some());
                if pipe.disconnect().is_err() {
                    break;
                }
            }
        }
        tokio::time::sleep(MEDIA_REFRESH).await;
    }
}

async fn display_activity(sink: EventSink, mut ipc: watch::Receiver<Option<Activity>>) {
    let mut manager = None;
    let mut displayed = None;
    loop {
        let rpc_activity = ipc.borrow_and_update().clone();
        let has_rpc = rpc_activity.is_some();
        let (next, cover) = match rpc_activity {
            Some(activity) => (Some(activity), None),
            None => tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(2), windows_activity(&mut manager, displayed.as_ref())) => {
                    match result {
                        Ok(Ok(activity)) => activity,
                        Ok(Err(_)) => {
                            manager = None;
                            (None, None)
                        }
                        Err(_) => (displayed.clone(), None),
                    }
                }
                _ = ipc.changed() => continue,
            },
        };
        if next != displayed {
            sink.send(BackendEvent::Activity(ActivityEvent::PulseSync(
                next.clone(),
            )));
            displayed = next;
        }
        if let Some(cover) = cover {
            sink.send(BackendEvent::Activity(ActivityEvent::PulseCover(Arc::new(
                cover,
            ))));
        }
        if has_rpc {
            if ipc.changed().await.is_err() {
                break;
            }
        } else if let Ok(Err(_)) = tokio::time::timeout(MEDIA_REFRESH, ipc.changed()).await {
            break;
        }
    }
}

fn is_yandex_music(source: &str) -> bool {
    [
        "ru.yandex.desktop.music",
        "com.squirrel.YandexMusic.YandexMusic",
        "YandexMusic",
        "YandexMusic.exe",
        "Yandex Music.exe",
        "Яндекс Музыка.exe",
    ]
    .iter()
    .any(|id| source.eq_ignore_ascii_case(id))
}

async fn windows_activity(
    manager: &mut Option<MediaSessionManager>,
    displayed: Option<&Activity>,
) -> windows::core::Result<(Option<Activity>, Option<egui::ColorImage>)> {
    if manager.is_none() {
        *manager = Some(MediaSessionManager::RequestAsync()?.await?);
    }
    let Some(manager) = manager else {
        return Ok((None, None));
    };
    let Some(session) = playing_yandex_session(manager)? else {
        return Ok((None, None));
    };
    let properties = session.TryGetMediaPropertiesAsync()?.await?;
    let title = properties.Title()?.to_string_lossy();
    let artist = properties.Artist()?.to_string_lossy();
    let activity = make_activity("Yandex Music", Some(&title), Some(&artist));
    let cover = if activity.is_some() && activity.as_ref() != displayed {
        tokio::time::timeout(Duration::from_millis(700), thumbnail(properties))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    Ok((activity, cover))
}

async fn thumbnail(properties: MediaProperties) -> Option<egui::ColorImage> {
    let (reader, size) = {
        let open = properties.Thumbnail().ok()?.OpenReadAsync().ok()?;
        let stream = open.await.ok()?;
        let size = stream.Size().ok()?;
        if size == 0 || size > 512 * 1024 {
            return None;
        }
        (
            DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?,
            size,
        )
    };
    let loaded = reader.LoadAsync(size as u32).ok()?.await.ok()?;
    if u64::from(loaded) != size {
        return None;
    }
    let mut bytes = vec![0; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    tokio::task::spawn_blocking(move || {
        crate::cache::images::decode(&bytes, 128, false)
            .ok()?
            .frames
            .into_iter()
            .next()
            .map(|f| f.image)
    })
    .await
    .ok()
    .flatten()
}

fn playing_yandex_session(
    manager: &MediaSessionManager,
) -> windows::core::Result<Option<MediaSession>> {
    Ok(manager.GetSessions()?.into_iter().find(|session| {
        session
            .SourceAppUserModelId()
            .is_ok_and(|id| is_yandex_music(&id.to_string_lossy()))
            && matches!(
                session
                    .GetPlaybackInfo()
                    .and_then(|info| info.PlaybackStatus()),
                Ok(PlaybackStatus::Playing)
            )
    }))
}

async fn read_frame(pipe: &mut (impl AsyncReadExt + Unpin), body: &mut Vec<u8>) -> io::Result<u32> {
    let op = pipe.read_u32_le().await?;
    let len = pipe.read_u32_le().await? as usize;
    if len > MAX_BODY {
        return Err(io::ErrorKind::InvalidData.into());
    }
    body.resize(len, 0);
    pipe.read_exact(body).await?;
    Ok(op)
}

async fn write_frame(
    pipe: &mut (impl AsyncWriteExt + Unpin),
    op: u32,
    body: &[u8],
) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        pipe.write_u32_le(op).await?;
        pipe.write_u32_le(body.len() as u32).await?;
        pipe.write_all(body).await
    })
    .await?
}

fn parse(body: &[u8]) -> io::Result<Value> {
    serde_json::from_slice(body).map_err(io::Error::other)
}

fn is_pulse_sync(hello: &Value) -> bool {
    hello["v"].as_u64() == Some(1) && hello["client_id"].as_str() == Some(PULSESYNC_ID)
}

async fn session(
    pipe: &mut NamedPipeServer,
    mut display: impl FnMut(Option<Activity>),
) -> io::Result<()> {
    let mut body = Vec::new();
    let op = tokio::time::timeout(Duration::from_secs(3), read_frame(pipe, &mut body)).await??;
    if op != HANDSHAKE || !is_pulse_sync(&parse(&body)?) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let ready = json!({
        "cmd": "DISPATCH", "evt": "READY", "nonce": null,
        "data": {"v": 1, "config": {"cdn_host": "cdn.discordapp.com", "api_endpoint": "//discord.com/api", "environment": "production"},
            "user": {"id": "0", "username": "Rustcord", "discriminator": "0", "avatar": null}}
    });
    write_frame(pipe, FRAME, ready.to_string().as_bytes()).await?;
    let mut current = None;
    loop {
        let op = read_frame(pipe, &mut body).await?;
        match op {
            CLOSE => return Ok(()),
            PING => write_frame(pipe, PONG, &body).await?,
            FRAME => {
                let command = parse(&body)?;
                if command["cmd"].as_str() != Some("SET_ACTIVITY") {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                let next = listening_activity(&command["args"]["activity"]);
                if next != current {
                    display(next.clone());
                    current = next;
                }
                let ack = json!({"cmd": "SET_ACTIVITY", "data": null, "evt": null, "nonce": command["nonce"].as_str()});
                write_frame(pipe, FRAME, ack.to_string().as_bytes()).await?;
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

fn listening_activity(value: &Value) -> Option<Activity> {
    if value["type"].as_u64() != Some(2) {
        return None;
    }
    let mut activity = make_activity(
        "Yandex Music",
        value["details"].as_str(),
        value["state"].as_str(),
    )?;
    let rich = activity.rich.as_mut()?;
    rich.large_image = value["assets"]["large_image"]
        .as_str()
        .filter(|image| {
            cover_url(image)
                || (image.len() <= 512
                    && (image.starts_with("mp:external/")
                        || !image.is_empty() && image.bytes().all(|b| b.is_ascii_digit())))
        })
        .map(str::to_owned);
    Some(activity)
}

fn make_activity(name: &str, title: Option<&str>, artist: Option<&str>) -> Option<Activity> {
    fn text(label: &str) -> Option<String> {
        let label: String = label
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect();
        (!label.is_empty()).then_some(label)
    }
    let details = title.and_then(text);
    let state = artist.and_then(text);
    if details.is_none() && state.is_none() {
        return None;
    }
    Some(Activity {
        kind: 2,
        name: name.into(),
        rich: Some(Box::new(RichPresence {
            details,
            state,
            application_id: Some(PULSESYNC_APP),
            ..Default::default()
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_stock_pulse_sync_application_and_listening_metadata() {
        assert!(is_pulse_sync(&json!({"v": 1, "client_id": PULSESYNC_ID})));
        assert!(!is_pulse_sync(&json!({"v": 1, "client_id": "123"})));
        assert!(!is_pulse_sync(&json!({"v": 2, "client_id": PULSESYNC_ID})));
        let activity = listening_activity(&json!({
            "type": 2, "details": "Test track", "state": "Test artist", "assets": {"large_image": "https://example.com"}
        })).unwrap();
        let rich = activity.rich.unwrap();
        assert_eq!(rich.details.as_deref(), Some("Test track"));
        assert_eq!(rich.state.as_deref(), Some("Test artist"));
        assert!(rich.large_image.is_none());
        let cover = "https://avatars.yandex.net/get-music-content/123/album/400x400";
        let with_cover = listening_activity(
            &json!({"type": 2, "details": "Track", "assets": {"large_image": cover}}),
        )
        .unwrap();
        assert_eq!(with_cover.rich.unwrap().large_image.as_deref(), Some(cover));
        assert!(!cover_url(
            "https://avatars.yandex.net/get-music-content/123/400x400?token=ignored"
        ));
        assert!(listening_activity(&Value::Null).is_none());
        assert!(listening_activity(&json!({"type": 0, "details": "Other"})).is_none());
    }

    #[test]
    fn recognizes_yandex_music_media_sessions() {
        assert!(is_yandex_music("ru.yandex.desktop.music"));
        assert!(is_yandex_music("YandexMusic.EXE"));
        assert!(!is_yandex_music("YandexBrowser.exe"));
        assert!(!is_yandex_music("com.squirrel.Discord.Discord"));
        assert!(make_activity("Yandex Music", Some("\t"), None).is_none());
    }

    #[tokio::test]
    async fn receives_and_clears_a_track_through_the_windows_pipe() {
        let name = format!(r"\\.\pipe\rustcord-pulse-test-{}", std::process::id());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
            .unwrap();
        let (events, rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            server.connect().await.unwrap();
            session(&mut server, |activity| events.send(activity).unwrap())
                .await
                .unwrap();
        });
        let mut client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(name)
            .unwrap();
        write_frame(
            &mut client,
            HANDSHAKE,
            json!({"v": 1, "client_id": PULSESYNC_ID})
                .to_string()
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut received = Vec::new();
        assert_eq!(read_frame(&mut client, &mut received).await.unwrap(), FRAME);
        assert_eq!(parse(&received).unwrap()["evt"], "READY");
        for (nonce, activity) in [
            (
                "playing",
                json!({"type": 2, "details": "Test track", "state": "Test artist"}),
            ),
            ("stop", Value::Null),
        ] {
            let body =
                json!({"cmd": "SET_ACTIVITY", "args": {"activity": activity}, "nonce": nonce});
            write_frame(&mut client, FRAME, body.to_string().as_bytes())
                .await
                .unwrap();
            assert_eq!(read_frame(&mut client, &mut received).await.unwrap(), FRAME);
            assert_eq!(parse(&received).unwrap()["nonce"], nonce);
        }
        write_frame(&mut client, CLOSE, &[]).await.unwrap();
        task.await.unwrap();
        assert!(matches!(rx.recv().unwrap(), Some(activity)
            if activity.rich.as_deref().and_then(|rich| rich.details.as_deref()) == Some("Test track")));
        assert!(rx.recv().unwrap().is_none());
    }
}
