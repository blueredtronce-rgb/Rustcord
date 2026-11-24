//! Opt-in Windows IPC adapter for PulseSync's listening activity.
//! Reads only SET_ACTIVITY, never authenticates or handles account credentials.

use crate::activity::ActivityEvent;
use crate::backend::{BackendEvent, EventSink};
use crate::model::{Activity, RichPresence};
use serde_json::{Value, json};
use std::{io, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

const PIPE: &str = r"\\.\pipe\discord-ipc-0";
const PULSESYNC_ID: &str = "1124055337234858005";
const MAX_BODY: usize = 16 * 1024;
const HANDSHAKE: u32 = 0;
const FRAME: u32 = 1;
const CLOSE: u32 = 2;
const PING: u32 = 3;
const PONG: u32 = 4;

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
    loop {
        // The first instance flag keeps this bridge from joining or taking over Discord's pipe.
        if let Ok(mut pipe) = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create(PIPE)
            && pipe.connect().await.is_ok()
        {
            let _ = session(&mut pipe, &sink).await;
            sink.send(BackendEvent::Activity(ActivityEvent::PulseSync(None)));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

async fn read_frame(pipe: &mut (impl AsyncReadExt + Unpin)) -> io::Result<(u32, Vec<u8>)> {
    let op = pipe.read_u32_le().await?;
    let len = pipe.read_u32_le().await? as usize;
    if len > MAX_BODY {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; len];
    pipe.read_exact(&mut body).await?;
    Ok((op, body))
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

async fn session(pipe: &mut NamedPipeServer, sink: &EventSink) -> io::Result<()> {
    let (op, body) = tokio::time::timeout(Duration::from_secs(3), read_frame(pipe)).await??;
    if op != HANDSHAKE || !is_pulse_sync(&parse(&body)?) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let ready = json!({
        "cmd": "DISPATCH", "evt": "READY", "nonce": null,
        "data": {"v": 1, "user": {"id": "0", "username": "Rustcord", "discriminator": "0", "avatar": null}}
    });
    write_frame(pipe, FRAME, ready.to_string().as_bytes()).await?;
    let mut current = None;
    loop {
        let (op, body) = read_frame(pipe).await?;
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
                    sink.send(BackendEvent::Activity(ActivityEvent::PulseSync(
                        next.clone(),
                    )));
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
    fn text(value: &Value) -> Option<String> {
        let label = value.as_str()?.trim();
        let label: String = label
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect();
        (!label.is_empty()).then_some(label)
    }
    if value["type"].as_u64() != Some(2) {
        return None;
    }
    let details = text(&value["details"]);
    let state = text(&value["state"]);
    if details.is_none() && state.is_none() {
        return None;
    }
    Some(Activity {
        kind: 2,
        name: "Yandex Music · PulseSync".into(),
        rich: Some(Box::new(RichPresence {
            details,
            state,
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
        assert!(listening_activity(&Value::Null).is_none());
        assert!(listening_activity(&json!({"type": 0, "details": "Other"})).is_none());
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
        let sink = EventSink::new(events, egui::Context::default());
        let task = tokio::spawn(async move {
            server.connect().await.unwrap();
            session(&mut server, &sink).await.unwrap();
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
        let (_, ready) = read_frame(&mut client).await.unwrap();
        assert_eq!(parse(&ready).unwrap()["evt"], "READY");
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
            let (_, ack) = read_frame(&mut client).await.unwrap();
            assert_eq!(parse(&ack).unwrap()["nonce"], nonce);
        }
        write_frame(&mut client, CLOSE, &[]).await.unwrap();
        task.await.unwrap();
        assert!(
            matches!(rx.recv().unwrap(), BackendEvent::Activity(ActivityEvent::PulseSync(Some(activity)))
            if activity.rich.as_deref().and_then(|rich| rich.details.as_deref()) == Some("Test track"))
        );
        assert!(matches!(
            rx.recv().unwrap(),
            BackendEvent::Activity(ActivityEvent::PulseSync(None))
        ));
    }
}
