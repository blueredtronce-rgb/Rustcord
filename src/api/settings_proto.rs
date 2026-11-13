//! Hand-written codec for the few `PreloadedUserSettings` fields Rustcord uses (FEATURES_NOTES
//! §3.1.3); no protobuf crate. Everything in and out is standard base64, as on the wire (READY
//! `user_settings_proto`, USER_SETTINGS_PROTO_UPDATE `settings.proto`, `GET/PATCH
//! /users/@me/settings-proto/1` `settings`).
//!
//! Wire types: varint (0), fixed64 (1), length-delimited (2), fixed32 (5); groups (3/4) are an
//! error. Wrapper types (`StringValue`, `Int64Value`, `UInt64Value`, `BoolValue`) are
//! sub-messages with the value in field 1. `repeated fixed64` accepts packed (wt 2) and unpacked
//! (wt 1). Unknown fields are skipped when decoding and **kept byte for byte** when editing: an
//! edit takes the current top-level field (11 or 14), replaces only the subfields it is about,
//! and returns a message holding just that one top-level field (the server resets unsent
//! subfields of a sent top-level field, and leaves unsent top-level fields alone). A top-level
//! sub-message that occurs more than once is merged (its payloads concatenated), as protobuf does.
//!
//! Test vectors (FEATURES_NOTES §3.1.3, re-verified): `{11:{1:StringValue("dnd")}}` =
//! `WgcKBQoDZG5k`; `{11:{1:"online", 2:{1:"hi",3:"❤"}}}` = `WhUKCAoGb25saW5lEgkKAmhpGgPinaQ=`;
//! `{14:{1:[{1:[1,2] packed,2:Int64Value(123),3:"A",4:UInt64Value(0xFF0000)},{1:[3]}]}}` =
//! `cjAKIgoQAQAAAAAAAAACAAAAAAAAABICCHsaAwoBQSIFCICA/AcKCgoIAwAAAAAAAAA=`.
//!
//! OWNERSHIP: settings engineer (docs/ACCOUNT_SERVER_DESIGN.md).

use crate::model::{CustomStatus, Emoji, GuildFolder, Id, StatusSettings, UserSettingsProto, UserStatus};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// `PreloadedUserSettings.status`.
const STATUS: u32 = 11;
/// `PreloadedUserSettings.guild_folders`.
const GUILD_FOLDERS: u32 = 14;

/// Decodes field 11 (`status`: 11.1 status, 11.2 custom status with 11.2.4 expiry) and field 14
/// (`guild_folders`: 14.1 folders) of a base64 `PreloadedUserSettings`; absent top-level fields
/// stay `None`. Unknown status strings decode as `status: None`.
pub fn decode(b64: &str) -> Result<UserSettingsProto, String> {
    let bytes = from_b64(b64)?;
    let top = parse(&bytes)?;
    let mut raw = std::collections::BTreeMap::<u32, Vec<u8>>::new();
    for f in &top {
        if let Some(b) = f.bytes() {
            raw.entry(f.num).or_default().extend_from_slice(b);
        }
    }
    Ok(UserSettingsProto {
        status: merged(&top, STATUS).map(|b| decode_status(&b)).transpose()?,
        guild_folders: merged(&top, GUILD_FOLDERS).map(|b| decode_folders(&b)).transpose()?,
        raw,
    })
}

// ----- any other option (Settings pages, docs/SETTINGS_NOTES.md): one value at a path of 1-2
// subfield numbers inside one top-level field, e.g. 6.17 compact mode or 6.29.2.

/// A setting's value as it sits in its message.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    /// `BoolValue` / `UInt32Value` wrapper: written even when false / 0.
    Wrapped(u64),
    /// Plain proto3 bool / enum / varint: omitted when 0 (= unset / default).
    Plain(u64),
    /// `StringValue` wrapper.
    Str(String),
    /// `repeated fixed64` (guild ids, e.g. 8.3 `restricted_guilds`): the whole list, written
    /// packed; empty = omitted.
    Ids(Vec<u64>),
}

/// The varint at `path` inside the bytes of one top-level field (`UserSettingsProto::raw`);
/// None when absent (use the documented default). An empty wrapper reads as 0.
pub fn read_varint(msg: &[u8], path: &[u32], wrapped: bool) -> Option<u64> {
    let fields = parse(msg).ok()?;
    let (&num, rest) = path.split_first()?;
    if !rest.is_empty() {
        return read_varint(&merged(&fields, num)?, rest, wrapped);
    }
    if wrapped {
        return wrapped_varint(&fields, num).ok()?;
    }
    fields.iter().rev().filter(|f| f.num == num).find_map(|f| if let Val::Varint(v) = f.val { Some(v) } else { None })
}

/// The `repeated fixed64` at `path` inside one top-level field's bytes; empty when absent.
pub fn read_ids(msg: &[u8], path: &[u32]) -> Vec<u64> {
    let (Ok(fields), Some((&num, rest))) = (parse(msg), path.split_first()) else { return Vec::new() };
    if !rest.is_empty() {
        return merged(&fields, num).map_or_else(Vec::new, |m| read_ids(&m, rest));
    }
    fixed64s(&fields, num).unwrap_or_default()
}

/// The `StringValue` at `path` inside one top-level field's bytes.
pub fn read_str(msg: &[u8], path: &[u32]) -> Option<String> {
    let fields = parse(msg).ok()?;
    let (&num, rest) = path.split_first()?;
    if !rest.is_empty() {
        return read_str(&merged(&fields, num)?, rest);
    }
    wrapped_str(&fields, num).ok()?
}

/// `msg` (one message's bytes) with the value at `path` replaced; every other field kept byte
/// for byte (a nested path rebuilds only the sub-messages on the way).
pub fn set_in(msg: &[u8], path: &[u32], value: &SettingValue) -> Result<Vec<u8>, String> {
    let (&num, rest) = path.split_first().ok_or("empty setting path")?;
    let fields = parse(msg)?;
    let mut out = Vec::with_capacity(msg.len() + 16);
    for f in fields.iter().filter(|f| f.num != num) {
        out.extend_from_slice(f.raw);
    }
    if !rest.is_empty() {
        put_bytes(&mut out, num, &set_in(&merged(&fields, num).unwrap_or_default(), rest, value)?);
        return Ok(out);
    }
    match value {
        SettingValue::Wrapped(v) => put_wrapped_varint(&mut out, num, *v),
        SettingValue::Plain(0) => {}
        SettingValue::Plain(v) => {
            key(&mut out, num, 0);
            put_varint(&mut out, *v);
        }
        SettingValue::Str(s) => put_wrapped_str(&mut out, num, s),
        SettingValue::Ids(ids) if ids.is_empty() => {}
        SettingValue::Ids(ids) => put_bytes(&mut out, num, &ids.iter().flat_map(|id| id.to_le_bytes()).collect::<Vec<_>>()),
    }
    Ok(out)
}

/// PATCH payload (base64) that saves `values` (path, value) in top-level field `top`: only that
/// field, its other subfields kept.
pub fn with_values(current: &str, top: u32, values: &[(Vec<u32>, SettingValue)]) -> Result<String, String> {
    let bytes = from_b64(current)?;
    let mut field = merged(&parse(&bytes)?, top).unwrap_or_default();
    for (path, v) in values {
        field = set_in(&field, path, v)?;
    }
    let mut out = Vec::with_capacity(field.len() + 6);
    put_bytes(&mut out, top, &field);
    Ok(STANDARD.encode(out))
}

/// PATCH payload (base64) that saves `status` (11.1 `StringValue`, `UserStatus::as_str`):
/// `current`'s field 11 with 11.1 replaced and 11.4 (`status_expires_at_ms`, the old status's
/// duration, which must not end the new one) dropped, every other 11.x kept; nothing else.
pub fn with_status(current: &str, status: UserStatus) -> Result<String, String> {
    edit(current, STATUS, &[1, 4], |o| put_wrapped_str(o, 1, status.as_str()))
}

/// PATCH payload (base64) that saves the custom status: `current`'s field 11 with 11.2 replaced
/// (`None` or nothing to show = removed) by text (11.2.1), emoji id (11.2.2, fixed64) or name
/// (11.2.3) and `expires_at_ms` (11.2.4, fixed64, omitted when None); every other 11.x kept.
pub fn with_custom_status(current: &str, custom: Option<&CustomStatus>, expires_at_ms: Option<u64>) -> Result<String, String> {
    edit(current, STATUS, &[2], |o| {
        let Some(c) = custom else { return };
        let mut m = Vec::new();
        if let Some(t) = c.text.as_deref().filter(|t| !t.is_empty()) {
            put_bytes(&mut m, 1, t.as_bytes());
        }
        if let Some(e) = &c.emoji {
            if let Some(id) = e.id {
                put_fixed64(&mut m, 2, id.0);
            }
            if let Some(n) = e.name.as_deref().filter(|n| !n.is_empty()) {
                put_bytes(&mut m, 3, n.as_bytes());
            }
        }
        if m.is_empty() {
            return;
        }
        if let Some(at) = expires_at_ms {
            put_fixed64(&mut m, 4, at);
        }
        put_bytes(o, 2, &m);
    })
}

/// PATCH payload (base64) that saves the server bar: `current`'s field 14 with every 14.1
/// replaced by `folders` (guild ids packed fixed64; `id` Int64Value, `name` StringValue, `color`
/// UInt64Value, each omitted when None); 14.2 and unknown 14.x kept.
pub fn with_guild_folders(current: &str, folders: &[GuildFolder]) -> Result<String, String> {
    edit(current, GUILD_FOLDERS, &[1], |o| {
        for f in folders {
            let mut m = Vec::new();
            if !f.guild_ids.is_empty() {
                key(&mut m, 1, 2);
                put_varint(&mut m, 8 * f.guild_ids.len() as u64);
                for g in &f.guild_ids {
                    m.extend_from_slice(&g.0.to_le_bytes());
                }
            }
            if let Some(id) = f.id {
                put_wrapped_varint(&mut m, 2, id as u64);
            }
            if let Some(name) = &f.name {
                put_wrapped_str(&mut m, 3, name);
            }
            if let Some(color) = f.color {
                put_wrapped_varint(&mut m, 4, u64::from(color));
            }
            put_bytes(o, 1, &m);
        }
    })
}

// ----- decoding the modelled fields

fn decode_status(b: &[u8]) -> Result<StatusSettings, String> {
    let f = parse(b)?;
    let status = wrapped_str(&f, 1)?.and_then(|s| UserStatus::parse(&s));
    let (mut custom_status, mut expires) = (None, None);
    if let Some(c) = merged(&f, 2) {
        let c = parse(&c)?;
        let text = string(&c, 1)?.filter(|t| !t.is_empty());
        let id = fixed64(&c, 2).filter(|v| *v != 0).map(Id);
        let name = string(&c, 3)?.filter(|n| !n.is_empty());
        let emoji = (id.is_some() || name.is_some()).then_some(Emoji { id, name, animated: false });
        if text.is_some() || emoji.is_some() {
            custom_status = Some(CustomStatus { text, emoji });
            expires = fixed64(&c, 4).filter(|v| *v != 0);
        }
    }
    Ok(StatusSettings { status, custom_status, custom_status_expires_at_ms: expires })
}

fn decode_folders(b: &[u8]) -> Result<Vec<GuildFolder>, String> {
    let entries = parse(b)?;
    let folders = entries.iter().filter(|f| f.num == 1).filter_map(Field::bytes);
    folders
        .map(|b| {
            let f = parse(b)?;
            Ok(GuildFolder {
                guild_ids: fixed64s(&f, 1)?.into_iter().map(Id).collect(),
                id: wrapped_varint(&f, 2)?.map(|v| v as i64),
                name: wrapped_str(&f, 3)?.filter(|n| !n.is_empty()),
                color: wrapped_varint(&f, 4)?.and_then(|v| u32::try_from(v).ok()),
            })
        })
        .collect()
}

// ----- wire format

#[derive(Clone, Copy, Debug, PartialEq)]
enum Val<'a> {
    Varint(u64),
    I64(u64),
    Len(&'a [u8]),
    /// fixed32: nothing Rustcord reads; only skipped (and kept).
    I32,
}

/// One field as it was on the wire: `raw` is its tag and value, kept verbatim by edits.
struct Field<'a> {
    num: u32,
    val: Val<'a>,
    raw: &'a [u8],
}

impl<'a> Field<'a> {
    fn bytes(&self) -> Option<&'a [u8]> {
        if let Val::Len(b) = self.val { Some(b) } else { None }
    }
}

fn from_b64(b64: &str) -> Result<Vec<u8>, String> {
    STANDARD.decode(b64.trim()).map_err(|_| "settings are not valid base64".into())
}

const TRUNCATED: &str = "settings data is cut off";

fn varint(b: &[u8], i: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i).ok_or(TRUNCATED)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("settings data has an overlong number".into())
}

fn take<'a>(b: &'a [u8], i: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let s = i.checked_add(n).and_then(|end| b.get(*i..end)).ok_or(TRUNCATED)?;
    *i += n;
    Ok(s)
}

/// One message level; nested messages stay bytes until asked for.
fn parse(b: &[u8]) -> Result<Vec<Field<'_>>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let tag = varint(b, &mut i)?;
        let num = u32::try_from(tag >> 3).ok().filter(|n| *n != 0).ok_or("settings data has a bad field number")?;
        let val = match tag & 7 {
            0 => Val::Varint(varint(b, &mut i)?),
            1 => Val::I64(u64::from_le_bytes(take(b, &mut i, 8)?.try_into().expect("8 bytes"))),
            2 => {
                let n = usize::try_from(varint(b, &mut i)?).map_err(|_| TRUNCATED)?;
                Val::Len(take(b, &mut i, n)?)
            }
            5 => {
                take(b, &mut i, 4)?;
                Val::I32
            }
            _ => return Err("settings data uses an unsupported encoding (group)".into()),
        };
        out.push(Field { num, val, raw: &b[start..i] });
    }
    Ok(out)
}

/// Every length-delimited field `num` concatenated (= merged sub-message); None when absent.
fn merged(fields: &[Field<'_>], num: u32) -> Option<Vec<u8>> {
    let mut parts = fields.iter().filter(|f| f.num == num).filter_map(Field::bytes).peekable();
    parts.peek()?;
    Some(parts.flatten().copied().collect())
}

/// Last length-delimited field `num` as UTF-8.
fn string(fields: &[Field<'_>], num: u32) -> Result<Option<String>, String> {
    let last = fields.iter().rev().filter(|f| f.num == num).find_map(Field::bytes);
    last.map(|b| String::from_utf8(b.to_vec()).map_err(|_| "settings text is not UTF-8".into())).transpose()
}

/// Last fixed64 field `num`.
fn fixed64(fields: &[Field<'_>], num: u32) -> Option<u64> {
    fields.iter().rev().filter(|f| f.num == num).find_map(|f| if let Val::I64(v) = f.val { Some(v) } else { None })
}

/// `repeated fixed64` field `num`, packed and/or unpacked.
fn fixed64s(fields: &[Field<'_>], num: u32) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    for f in fields.iter().filter(|f| f.num == num) {
        match f.val {
            Val::I64(v) => out.push(v),
            Val::Len(b) => {
                let (ids, rest) = b.as_chunks::<8>();
                if !rest.is_empty() {
                    return Err("settings data has a bad id list".into());
                }
                out.extend(ids.iter().map(|c| u64::from_le_bytes(*c)));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `StringValue` field `num` (an empty wrapper is "").
fn wrapped_str(fields: &[Field<'_>], num: u32) -> Result<Option<String>, String> {
    let Some(w) = merged(fields, num) else { return Ok(None) };
    Ok(Some(string(&parse(&w)?, 1)?.unwrap_or_default()))
}

/// `Int64Value` / `UInt64Value` field `num` (an empty wrapper is 0).
fn wrapped_varint(fields: &[Field<'_>], num: u32) -> Result<Option<u64>, String> {
    let Some(w) = merged(fields, num) else { return Ok(None) };
    let inner = parse(&w)?;
    let v = inner.iter().rev().filter(|f| f.num == 1).find_map(|f| if let Val::Varint(v) = f.val { Some(v) } else { None });
    Ok(Some(v.unwrap_or(0)))
}

// ----- encoding

/// `current` reduced to top-level field `num`: subfields in `replace` dropped, the others kept
/// byte for byte, then `add` appended. Base64 of `{num: …}`.
fn edit(current: &str, num: u32, replace: &[u32], add: impl FnOnce(&mut Vec<u8>)) -> Result<String, String> {
    let bytes = from_b64(current)?;
    let old = merged(&parse(&bytes)?, num).unwrap_or_default();
    let mut inner = Vec::with_capacity(old.len() + 64);
    for f in parse(&old)?.iter().filter(|f| !replace.contains(&f.num)) {
        inner.extend_from_slice(f.raw);
    }
    add(&mut inner);
    let mut out = Vec::with_capacity(inner.len() + 6);
    put_bytes(&mut out, num, &inner);
    Ok(STANDARD.encode(out))
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn key(out: &mut Vec<u8>, num: u32, wire_type: u8) {
    put_varint(out, u64::from(num) << 3 | u64::from(wire_type));
}

fn put_bytes(out: &mut Vec<u8>, num: u32, b: &[u8]) {
    key(out, num, 2);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_fixed64(out: &mut Vec<u8>, num: u32, v: u64) {
    key(out, num, 1);
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_wrapped_str(out: &mut Vec<u8>, num: u32, s: &str) {
    let mut w = Vec::with_capacity(s.len() + 2);
    put_bytes(&mut w, 1, s.as_bytes());
    put_bytes(out, num, &w);
}

fn put_wrapped_varint(out: &mut Vec<u8>, num: u32, v: u64) {
    let mut w = Vec::with_capacity(11);
    key(&mut w, 1, 0);
    put_varint(&mut w, v);
    put_bytes(out, num, &w);
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "WgcKBQoDZG5k";
    const V2: &str = "WhUKCAoGb25saW5lEgkKAmhpGgPinaQ=";
    const V3: &str = "cjAKIgoQAQAAAAAAAAACAAAAAAAAABICCHsaAwoBQSIFCICA/AcKCgoIAwAAAAAAAAA=";

    fn heart() -> CustomStatus {
        CustomStatus { text: Some("hi".into()), emoji: Some(Emoji { id: None, name: Some("❤".into()), animated: false }) }
    }

    fn v3_folders() -> Vec<GuildFolder> {
        vec![
            GuildFolder { id: Some(123), name: Some("A".into()), color: Some(0xFF0000), guild_ids: vec![Id(1), Id(2)] },
            GuildFolder { guild_ids: vec![Id(3)], ..Default::default() },
        ]
    }

    #[test]
    fn vectors_encode() {
        assert_eq!(with_status("", UserStatus::Dnd).unwrap(), V1);
        let online = with_status("", UserStatus::Online).unwrap();
        assert_eq!(with_custom_status(&online, Some(&heart()), None).unwrap(), V2);
        assert_eq!(with_guild_folders("", &v3_folders()).unwrap(), V3);
    }

    #[test]
    fn vectors_decode() {
        let s = decode(V1).unwrap();
        assert_eq!(s.status.unwrap().status, Some(UserStatus::Dnd));
        assert_eq!(s.guild_folders, None);

        let s = decode(V2).unwrap().status.unwrap();
        assert_eq!(s.status, Some(UserStatus::Online));
        assert_eq!(s.custom_status, Some(heart()));
        assert_eq!(s.custom_status_expires_at_ms, None);

        let s = decode(V3).unwrap();
        assert_eq!(s.status, None);
        assert_eq!(s.guild_folders, Some(v3_folders()));
        assert_eq!(decode("").unwrap(), UserSettingsProto::default());
    }

    #[test]
    fn custom_status_with_custom_emoji_and_expiry_round_trips() {
        let c = CustomStatus { text: None, emoji: Some(Emoji { id: Some(Id(42)), name: Some("blob".into()), animated: false }) };
        let s = decode(&with_custom_status(V1, Some(&c), Some(1_800_000_000_000)).unwrap()).unwrap().status.unwrap();
        assert_eq!((s.status, s.custom_status, s.custom_status_expires_at_ms), (Some(UserStatus::Dnd), Some(c), Some(1_800_000_000_000)));
        // Clearing (or nothing to show) removes 11.2 and keeps 11.1.
        assert_eq!(with_custom_status(V2, None, Some(5)).unwrap(), with_status("", UserStatus::Online).unwrap());
        let empty = CustomStatus::default();
        assert_eq!(with_custom_status(V2, Some(&empty), None).unwrap(), with_status("", UserStatus::Online).unwrap());
    }

    #[test]
    fn edits_keep_unknown_subfields_and_drop_other_top_level_fields() {
        // {6:{14:BoolValue(true)}, 11:{3:BoolValue(true), 1:"idle", 4:fixed64 9, 99:varint 0 (non-minimal)}, 14:{2:[7] packed, 1:{1:[5]}}}
        let mut cur = Vec::new();
        put_bytes(&mut cur, 6, &[0x72, 0x02, 0x08, 0x01]);
        let unknown11: &[u8] = &[0x1a, 0x02, 0x08, 0x01, 0x98, 0x06, 0x80, 0x00];
        let mut f11 = unknown11[..4].to_vec();
        put_wrapped_str(&mut f11, 1, "idle");
        put_fixed64(&mut f11, 4, 9);
        f11.extend_from_slice(&unknown11[4..]);
        put_bytes(&mut cur, 11, &f11);
        let positions: &[u8] = &[0x12, 0x08, 7, 0, 0, 0, 0, 0, 0, 0];
        let mut f14 = positions.to_vec();
        put_bytes(&mut f14, 1, &[0x0a, 0x08, 5, 0, 0, 0, 0, 0, 0, 0]);
        put_bytes(&mut cur, 14, &f14);
        let cur = STANDARD.encode(cur);

        let mut want = unknown11.to_vec();
        put_wrapped_str(&mut want, 1, "dnd");
        let mut want_top = Vec::new();
        put_bytes(&mut want_top, 11, &want);
        assert_eq!(STANDARD.decode(with_status(&cur, UserStatus::Dnd).unwrap()).unwrap(), want_top);

        let f = vec![GuildFolder { guild_ids: vec![Id(8)], ..Default::default() }];
        let out = STANDARD.decode(with_guild_folders(&cur, &f).unwrap()).unwrap();
        let mut want = positions.to_vec();
        put_bytes(&mut want, 1, &[0x0a, 0x08, 8, 0, 0, 0, 0, 0, 0, 0]);
        let mut want_top = Vec::new();
        put_bytes(&mut want_top, 14, &want);
        assert_eq!(out, want_top);

        let d = decode(&cur).unwrap();
        assert_eq!(d.status.unwrap().status, Some(UserStatus::Idle));
        assert_eq!(d.guild_folders.unwrap()[0].guild_ids, vec![Id(5)]);
    }

    #[test]
    fn packed_and_unpacked_ids_and_odd_values() {
        // Folder: 1: fixed64 4 (unpacked), 1: [5, 6] packed, 2: Int64Value(-1), 3: StringValue(""), 4: UInt64Value(2^40).
        let mut folder = Vec::new();
        put_fixed64(&mut folder, 1, 4);
        let mut packed = 5u64.to_le_bytes().to_vec();
        packed.extend_from_slice(&6u64.to_le_bytes());
        put_bytes(&mut folder, 1, &packed);
        put_wrapped_varint(&mut folder, 2, -1i64 as u64);
        put_wrapped_str(&mut folder, 3, "");
        put_wrapped_varint(&mut folder, 4, 1 << 40);
        let mut f14 = Vec::new();
        put_bytes(&mut f14, 1, &folder);
        let mut top = Vec::new();
        put_bytes(&mut top, 14, &f14);
        // An unknown status string and a fixed32 field decode without error.
        put_bytes(&mut top, 11, &[0x0a, 0x05, 0x0a, 0x03, b'a', b'f', b'k', 0x2d, 1, 2, 3, 4]);
        let d = decode(&STANDARD.encode(&top)).unwrap();
        let f = &d.guild_folders.unwrap()[0];
        assert_eq!((f.guild_ids.clone(), f.id, f.name.clone(), f.color), (vec![Id(4), Id(5), Id(6)], Some(-1), None, None));
        assert_eq!(d.status.unwrap(), StatusSettings::default());
    }

    #[test]
    fn setting_vectors() {
        // docs/SETTINGS_NOTES.md §0 (verified with Python).
        let one = |top, path: &[u32], v| with_values("", top, &[(path.to_vec(), v)]).unwrap();
        assert_eq!(one(13, &[2], SettingValue::Plain(1)), "agIQAQ==");
        assert_eq!(one(13, &[1], SettingValue::Plain(2)), "agIIAg==");
        assert_eq!(one(6, &[17], SettingValue::Wrapped(1)), "MgWKAQIIAQ==");
        assert_eq!(one(6, &[17], SettingValue::Wrapped(0)), "MgWKAQIIAA==");
        // Both encodings of false read as 0; a plain 0 is omitted (unset).
        for b64 in ["MgOKAQA=", "MgWKAQIIAA=="] {
            assert_eq!(read_varint(&decode(b64).unwrap().raw[&6], &[17], true), Some(0));
        }
        assert_eq!(read_varint(&decode("mgECCgA=").unwrap().raw[&19], &[1], true), Some(0));
        assert_eq!(one(13, &[2], SettingValue::Plain(0)), STANDARD.encode([0x6a, 0x00]));
        let d = decode("agIQAQ==").unwrap();
        assert_eq!((read_varint(&d.raw[&13], &[2], false), read_varint(&d.raw[&13], &[1], false)), (Some(1), None));
    }

    #[test]
    fn id_lists_round_trip() {
        // {8:{3:[1, 2^40]}}: packed fixed64s; then the list emptied (omitted), 8.4 kept.
        let cur = with_values("", 8, &[(vec![4], SettingValue::Plain(1)), (vec![3], SettingValue::Ids(vec![1, 1 << 40]))]).unwrap();
        let f = &decode(&cur).unwrap().raw[&8];
        assert_eq!(read_ids(f, &[3]), [1, 1 << 40]);
        let out = with_values(&cur, 8, &[(vec![3], SettingValue::Ids(Vec::new()))]).unwrap();
        let f = &decode(&out).unwrap().raw[&8];
        assert!(read_ids(f, &[3]).is_empty());
        assert_eq!(read_varint(f, &[4], false), Some(1));
    }

    #[test]
    fn nested_setting_edits_keep_siblings() {
        // {6:{17:Bool(true), 29:{1:1, 3:3}}} -> 6.29.2 = 2, then 12.1 locale.
        let cur = with_values("", 6, &[(vec![17], SettingValue::Wrapped(1)), (vec![29, 1], SettingValue::Plain(1)), (vec![29, 3], SettingValue::Plain(3))]).unwrap();
        let out = with_values(&cur, 6, &[(vec![29, 2], SettingValue::Plain(2))]).unwrap();
        let f = &decode(&out).unwrap().raw[&6];
        let got: Vec<_> = [&[17][..], &[29, 1], &[29, 2], &[29, 3]].iter().map(|p| read_varint(f, p, p.len() == 1)).collect();
        assert_eq!(got, [Some(1), Some(1), Some(2), Some(3)]);
        let loc = with_values(&out, 12, &[(vec![1], SettingValue::Str("de".into()))]).unwrap();
        let d = decode(&loc).unwrap();
        assert_eq!((read_str(&d.raw[&12], &[1]).as_deref(), d.raw.contains_key(&6)), (Some("de"), false));
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(decode("not base64!").is_err());
        assert!(decode(&STANDARD.encode([0x5a, 0x07, 0x0a, 0x05, 0x0a])).is_err(), "truncated");
        assert!(decode(&STANDARD.encode([0x5a, 0x02, 0x0b, 0x0c])).is_err(), "group inside 11");
        assert!(decode(&STANDARD.encode([0x0b])).is_err(), "top-level group");
        assert!(decode(&STANDARD.encode([0x72, 0x07, 0x0a, 0x05, 0x0a, 0x03, 1, 2, 3])).is_err(), "packed ids not a multiple of 8");
        assert!(with_status(&STANDARD.encode([0x5a, 0x05]), UserStatus::Online).is_err());
        assert!(decode(&STANDARD.encode([0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01])).is_err(), "overlong varint");
    }
}
