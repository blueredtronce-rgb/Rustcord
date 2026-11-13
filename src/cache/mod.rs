//! Disk cache: a JSON snapshot of guilds/channels/DMs/read states for instant startup,
//! plus downloaded images (see `images`).
//!
//! The snapshot never contains the token or anything secret.
//!
//! OWNERSHIP: `Snapshot` fields + pub fn signatures are a shared contract; bodies belong to
//! the cache implementer.

pub mod images;

use crate::model::*;
use serde::{Deserialize, Serialize};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub version: u32,
    pub me: Option<User>,
    /// Users referenced by DMs (and authors of cached messages).
    pub users: Vec<User>,
    /// Guilds in sidebar order, *with* their channels.
    pub guilds: Vec<Guild>,
    pub private_channels: Vec<Channel>,
    pub my_members: Vec<(Id, Member)>,
    pub read_states: Vec<ReadState>,
    pub guild_settings: Vec<GuildSettings>,
    pub selected_guild: Option<Id>,
    pub selected_channel: Option<Id>,
    pub last_channel_per_guild: Vec<(Id, Id)>,
    /// Last messages of a few recently viewed channels.
    pub recent_messages: Vec<(Id, Vec<Message>)>,
}

/// Load the snapshot (None if missing, unreadable, or a different version).
pub fn load_snapshot() -> Option<Snapshot> {
    let path = crate::config::paths().snapshot_file();
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Snapshot>(&bytes) {
        Ok(snap) if snap.version == SNAPSHOT_VERSION => Some(snap),
        Ok(_) => None,
        Err(e) => {
            eprintln!("rustcord: ignoring corrupt cache {}: {e}", path.display());
            None
        }
    }
}

/// Serialize + atomically write. Call from a background thread; it does blocking I/O.
pub fn save_snapshot(snap: &Snapshot) -> Result<(), String> {
    // The version stamp is what load_snapshot checks, so never write a file without it.
    let fixed;
    let snap = if snap.version == SNAPSHOT_VERSION {
        snap
    } else {
        fixed = Snapshot { version: SNAPSHOT_VERSION, ..snap.clone() };
        &fixed
    };
    let bytes = serde_json::to_vec(snap).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&crate::config::paths().snapshot_file(), &bytes)
}

/// Delete snapshot and image cache (logout).
pub fn clear_all() {
    let paths = crate::config::paths();
    let _ = std::fs::remove_file(paths.snapshot_file());
    let _ = std::fs::remove_dir_all(paths.images_dir());
}
