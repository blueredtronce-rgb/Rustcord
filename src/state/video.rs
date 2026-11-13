//! What `VoiceEvent::Video` reported (read by the video UI). Pure data, no egui: frames are not
//! here but in `BackendHandle::video` (docs/VIDEO_DESIGN.md "UI hook-up").
//!
//! OWNERSHIP: contract owner of docs/VIDEO_DESIGN.md.

use crate::voice::video::{CameraInfo, MonitorInfo, VideoEvent, VideoKey, WindowInfo};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct VideoStatus {
    pub camera_on: bool,
    /// My Go Live's stream key while I share my screen.
    pub screen_share: Option<String>,
    /// Go Live clicked, `ScreenShareStarted` not here yet (connecting can take ~10 s): the
    /// screen button is lit already, so a click cancels it (`sharing`).
    pub share_starting: bool,
    /// From the last `ListCameras` / `ListScreenSources`.
    pub cameras: Vec<CameraInfo>,
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,
    /// Videos that can be shown now (cameras in the call, watched streams) with their last
    /// known size; 0×0 until the first frame is decoded.
    pub remote: HashMap<VideoKey, (u32, u32)>,
    /// `RemoteVideoEnded` keys not yet seen by the call view (`ui::call::sync` drains them): a
    /// camera that went off and on between two frames must be asked for again.
    pub ended: Vec<VideoKey>,
}

impl VideoStatus {
    /// Apply one event; returns user-facing text worth a toast.
    pub fn apply(&mut self, ev: VideoEvent) -> Option<String> {
        match ev {
            VideoEvent::CameraStarted => self.camera_on = true,
            VideoEvent::CameraStopped => self.camera_on = false,
            VideoEvent::ScreenShareStarted { stream_key } => (self.screen_share, self.share_starting) = (Some(stream_key), false),
            VideoEvent::ScreenShareStopped => (self.screen_share, self.share_starting) = (None, false),
            VideoEvent::Sources { monitors, windows } => (self.monitors, self.windows) = (monitors, windows),
            VideoEvent::Cameras(cameras) => self.cameras = cameras,
            VideoEvent::RemoteVideo { key, width, height } => _ = self.remote.insert(key, (width, height)),
            VideoEvent::RemoteVideoEnded { key } => {
                self.remote.remove(&key);
                self.ended.push(key);
            }
            VideoEvent::Error(e) => return Some(e),
        }
        None
    }

    /// Out of voice: nothing of the call is on any more; the device lists stay.
    pub fn end_call(&mut self) {
        self.camera_on = false;
        self.screen_share = None;
        self.share_starting = false;
        self.remote.clear();
    }

    /// Sharing or about to (the screen button is lit and stops it).
    pub fn sharing(&self) -> bool {
        self.screen_share.is_some() || self.share_starting
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Id;

    #[test]
    fn video_events_update_status() {
        let mut s = VideoStatus::default();
        assert_eq!(s.apply(VideoEvent::CameraStarted), None);
        s.apply(VideoEvent::ScreenShareStarted { stream_key: "call:1:2".into() });
        s.apply(VideoEvent::RemoteVideo { key: VideoKey::User(Id(5)), width: 0, height: 0 });
        s.apply(VideoEvent::RemoteVideo { key: VideoKey::User(Id(5)), width: 640, height: 360 });
        s.apply(VideoEvent::RemoteVideo { key: VideoKey::Stream("call:1:3".into()), width: 0, height: 0 });
        s.apply(VideoEvent::RemoteVideoEnded { key: VideoKey::Stream("call:1:3".into()) });
        assert_eq!(s.remote, HashMap::from([(VideoKey::User(Id(5)), (640, 360))]));
        assert_eq!(s.ended, [VideoKey::Stream("call:1:3".into())]);
        s.apply(VideoEvent::Cameras(vec![CameraInfo { id: "cam".into(), name: "Cam".into() }]));
        assert_eq!(s.apply(VideoEvent::Error("no camera".into())).as_deref(), Some("no camera"));
        assert!(s.camera_on && s.screen_share.is_some());
        s.end_call();
        assert!(!s.camera_on && s.screen_share.is_none() && s.remote.is_empty());
        assert_eq!(s.cameras.len(), 1, "device lists survive the call");
        s.apply(VideoEvent::CameraStarted);
        s.apply(VideoEvent::CameraStopped);
        assert!(!s.camera_on);
        // A pending Go Live counts as sharing until it starts or stops.
        s.share_starting = true;
        assert!(s.sharing());
        s.apply(VideoEvent::ScreenShareStarted { stream_key: "call:1:2".into() });
        assert!(!s.share_starting && s.sharing());
        s.apply(VideoEvent::ScreenShareStopped);
        assert!(!s.sharing());
        s.share_starting = true;
        s.end_call();
        assert!(!s.sharing());
    }
}
