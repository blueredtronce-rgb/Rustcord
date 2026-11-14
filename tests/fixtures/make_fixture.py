"""Generates tests/fixtures/home/cache/snapshot.json: fake servers/DMs/messages for offline
screenshots and startup/RAM measurements (RUSTCORD_HOME=tests/fixtures/home RUSTCORD_OFFLINE=1).
All names and ids are invented.

`python make_fixture.py media` only runs `seed_media()` (the snapshot stays as it is)."""
import datetime, json, math, os, struct, sys, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
MEDIA = os.path.join(HERE, "media")
# The URLs of RUSTCORD_MSG_DEMO=media (src/ui/viewer.rs `demo`) and the fixture file behind each.
MEDIA_URLS = [
    ("https://cdn.discordapp.com/attachments/1/1/demo.png", "demo.png"),
    ("https://cdn.discordapp.com/attachments/1/2/demo.gif", "demo.gif"),
    ("https://cdn.discordapp.com/attachments/1/3/demo.mp4", "demo.mp4"),
    ("https://images-ext-1.discordapp.net/external/demo/gifv.mp4", "demo.mp4"),
]


def fnv1a64(data):  # as src/cache/images.rs
    h = 0xCBF29CE484222325
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def png(w, h, pixel):
    """RGB PNG; rows use the Sub filter so flat areas and gradients compress well."""
    raw = bytearray()
    for y in range(h):
        row = [pixel(x, y) for x in range(w)]
        raw.append(1)
        prev = (0, 0, 0)
        for p in row:
            raw.extend((p[i] - prev[i]) & 255 for i in range(3))
            prev = p
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")


def lzw(data, min_size=8):
    """GIF LZW (variable code size, a clear code when the table is full)."""
    clear, eoi = 1 << min_size, (1 << min_size) + 1
    out, acc, nbits = bytearray(), 0, 0
    state = {"size": min_size + 1, "next": eoi + 1}

    def emit(code):
        nonlocal acc, nbits
        acc |= code << nbits
        nbits += state["size"]
        while nbits >= 8:
            out.append(acc & 255)
            acc >>= 8
            nbits -= 8
        if state["next"] > (1 << state["size"]) - 1 and state["size"] < 12:
            state["size"] += 1

    table = {bytes([i]): i for i in range(clear)}
    emit(clear)
    w = b""
    for c in data:
        wc = w + bytes([c])
        if wc in table:
            w = wc
            continue
        emit(table[w])
        if state["next"] < 4096:
            table[wc] = state["next"]
            state["next"] += 1
        else:
            emit(clear)
            table = {bytes([i]): i for i in range(clear)}
            state["size"], state["next"] = min_size + 1, eoi + 1
        w = bytes([c])
    if w:
        emit(table[w])
    emit(eoi)
    if nbits:
        out.append(acc & 255)
    return bytes(out)


def gif(w, h, frames, palette, delay_cs):
    """Looping GIF89a of palette-index frames."""
    pal = b"".join(bytes(c) for c in palette) + b"\0" * (3 * (256 - len(palette)))
    g = b"GIF89a" + struct.pack("<HHBBB", w, h, 0xF7, 0, 0) + pal
    g += b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00"
    for f in frames:
        g += b"\x21\xf9\x04\x00" + struct.pack("<H", delay_cs) + b"\x00\x00"
        g += b"\x2c" + struct.pack("<HHHHB", 0, 0, w, h, 0) + b"\x08"
        data = lzw(f)
        for i in range(0, len(data), 255):
            block = data[i:i + 255]
            g += bytes([len(block)]) + block
        g += b"\x00"
    return g + b"\x3b"


def seed_media():
    """Draws tests/fixtures/media/demo.png and demo.gif (demo.mp4 is the media probe's
    self-generated clip) and copies them into home/cache/images under the demo URLs' cache names,
    so RUSTCORD_MSG_DEMO=media plays and shows them offline."""
    os.makedirs(MEDIA, exist_ok=True)

    def landscape(x, y):
        # Sky gradient, a sun, two hills.
        if (x - 700) ** 2 + (y - 170) ** 2 < 70 ** 2:
            return (250, 214, 140)
        if y > 430 + 60 * math.sin(x / 140):
            return (52, 92, 74)
        if y > 380 + 40 * math.sin(x / 90 + 1.5):
            return (82, 128, 98)
        t = y / 600
        return (int(96 + 120 * t), int(134 + 90 * t), int(178 + 50 * t))

    with open(os.path.join(MEDIA, "demo.png"), "wb") as f:
        f.write(png(960, 600, landscape))
    w, h = 160, 100
    frames = []
    for n in range(3):
        px = bytearray(w * h)
        cx = 40 + 40 * n
        for y in range(h):
            for x in range(w):
                px[y * w + x] = 1 if (x - cx) ** 2 + (y - 50) ** 2 < 22 ** 2 else (2 if y > 88 and x < (n + 1) * w // 3 else 0)
        frames.append(bytes(px))
    with open(os.path.join(MEDIA, "demo.gif"), "wb") as f:
        f.write(gif(w, h, frames, [(30, 36, 44), (96, 180, 220), (220, 226, 232)], 40))
    out = os.path.join(HERE, "home", "cache", "images")
    os.makedirs(out, exist_ok=True)
    for url, name in MEDIA_URLS:
        with open(os.path.join(MEDIA, name), "rb") as src, open(os.path.join(out, "%016x" % fnv1a64(url.encode())), "wb") as dst:
            dst.write(src.read())
    print("seeded", len(MEDIA_URLS), "media files into", out)


if __name__ == "__main__" and sys.argv[1:] == ["media"]:
    seed_media()
    sys.exit(0)

EPOCH = 1420070400000
def sf(ms, n=0):  # snowflake from unix ms
    return str(((ms - EPOCH) << 22) | n)

base_ms = int(datetime.datetime(2026, 11, 10, 9, 0, tzinfo=datetime.timezone.utc).timestamp() * 1000)
def iso(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).isoformat().replace("+00:00", "+00:00")

counter = [0]
def nid(ms=None):
    counter[0] += 1
    return sf(ms or base_ms - 10**9, counter[0])

def user(name, gname=None):
    return {"id": nid(), "username": name, "global_name": gname, "discriminator": "0", "avatar": None, "bot": False}

me = user("ferris_dev", "Ferris")
alice, bob, cara, dan = user("alice", "Alice"), user("bobby", "Bob"), user("cara.k", "Cara"), user("dan_ops", None)
users = [alice, bob, cara, dan]

def role(name, color, pos, perms="104324673"):
    return {"id": nid(), "name": name, "color": color, "position": pos, "permissions": perms, "hoist": True}

def channel(name, kind, pos, gid, parent=None, topic=None):
    return {"id": nid(), "type": kind, "guild_id": gid, "name": name, "position": pos, "parent_id": parent,
            "topic": topic, "last_message_id": None, "permission_overwrites": []}

guilds, read_states, recent = [], [], []
def guild(name, cats):
    gid = nid()
    everyone = {"id": gid, "name": "@everyone", "color": 0, "position": 0, "permissions": "104324673", "hoist": False}
    roles = [everyone, role("Maintainers", 0xE8743B, 2), role("Contributors", 0x4FB286, 1)]
    chans = []
    for ci, (cat, kids) in enumerate(cats):
        c = channel(cat, 4, ci, gid)
        chans.append(c)
        for ki, (kname, kind, topic) in enumerate(kids):
            chans.append(channel(kname, kind, ki, gid, c["id"], topic))
    g = {"id": gid, "name": name, "icon": None, "owner_id": alice["id"], "roles": roles, "emojis": [],
         "channels": chans, "default_message_notifications": 1, "unavailable": False}
    guilds.append(g)
    return g

g1 = guild("Rust Workshop", [("Text", [("general", 0, "Talk about anything Rust"), ("help", 0, "Ask questions"), ("showcase", 5, None)]),
                             ("Voice", [("Lounge", 2, None)])])
g2 = guild("Pixel Garden", [("Chat", [("lobby", 0, None), ("art-share", 0, None)])])
g3 = guild("Night Owls", [("Main", [("chatter", 0, None), ("music", 0, None)])])

general = g1["channels"][1]
msgs = []
lines = [
    (alice, "Morning all! Anyone tried the new **async closures** yet?"),
    (bob, "Yes! They make `tokio::spawn` code *much* nicer."),
    (bob, "Example:\n```rust\nlet f = async |x: u32| x * 2;\nprintln!(\"{}\", f(21).await);\n```"),
    (cara, "> They make tokio code much nicer\nAgreed, also ~~no more~~ fewer boxed futures"),
    (me, "Nice. I'm building a tiny chat client with egui, startup is under a second so far"),
    (alice, "<@%s> that's awesome, share it in <#%s> when it's ready" % (me["id"], g1["channels"][3]["id"])),
    (dan, "||spoiler: it's written in Rust|| :)"),
    (cara, "Links work too: https://www.rust-lang.org and [the book](https://doc.rust-lang.org/book/)"),
    (bob, "# Release notes\n- faster builds\n- smaller binaries\n-# (subtext) beta only"),
    (alice, "__underline__, ***bold italic***, and a timestamp <t:1793000000:R>"),
    (me, "Thanks <@%s>!" % alice["id"]),
]
t = base_ms
for i, (u, content) in enumerate(lines * 3):
    t += 97_000 if i % 4 else 1_900_000
    mid = sf(t, i)
    m = {"id": mid, "channel_id": general["id"], "guild_id": g1["id"], "author": u, "content": content,
         "timestamp": iso(t), "edited_timestamp": None, "type": 0, "flags": 0, "mentions": [],
         "mention_roles": [], "mention_everyone": False, "attachments": [], "embeds": [], "reactions": [],
         "pinned": False}
    if i == 4:
        m["reactions"] = [{"emoji": {"id": None, "name": "\U0001F525"}, "count": 3, "me": True},
                          {"emoji": {"id": None, "name": "\U0001F980"}, "count": 1, "me": False}]
    if i == 7:
        m["embeds"] = [{"type": "rich", "title": "Rust Programming Language", "url": "https://www.rust-lang.org",
                        "description": "A language empowering everyone to build **reliable** and efficient software.",
                        "color": 0xE8743B, "fields": [{"name": "Edition", "value": "2024", "inline": True}]}]
    if i == 12:
        m["type"] = 19
        m["referenced_message"] = msgs[10]
        m["message_reference"] = {"message_id": msgs[10]["id"], "channel_id": general["id"]}
    if "<@%s>" % me["id"] in content:
        m["mentions"] = [me]
    msgs.append(m)
general["last_message_id"] = msgs[-1]["id"]
recent.append([general["id"], msgs])
read_states.append({"id": general["id"], "last_message_id": msgs[-4]["id"], "mention_count": 1, "read_state_type": 0})
# other channels: unread markers
for g in (g2, g3):
    c = g["channels"][1]
    c["last_message_id"] = sf(base_ms + 5_000_000, 1)
    read_states.append({"id": c["id"], "last_message_id": sf(base_ms, 1), "mention_count": 0, "read_state_type": 0})

dms = []
for u in (alice, bob, cara):
    dms.append({"id": nid(), "type": 1, "name": None, "position": 0, "last_message_id": sf(base_ms + 3_000_000, 2),
                "permission_overwrites": [], "recipient_ids": [u["id"]]})
dms.append({"id": nid(), "type": 3, "name": "weekend plans", "position": 0, "last_message_id": sf(base_ms, 3),
            "permission_overwrites": [], "recipient_ids": [alice["id"], bob["id"], dan["id"]]})

snap = {
    "version": 1, "me": me, "users": users, "guilds": guilds, "private_channels": dms,
    "my_members": [[g["id"], {"nick": None, "roles": [g["roles"][1]["id"]]}] for g in guilds],
    "read_states": read_states,
    "guild_settings": [{"guild_id": None, "muted": False, "message_notifications": 0, "suppress_everyone": False,
                        "suppress_roles": False, "channel_overrides": []}],
    "selected_guild": g1["id"], "selected_channel": general["id"],
    "last_channel_per_guild": [[g1["id"], general["id"]]],
    "recent_messages": recent,
}
out = os.path.join(os.path.dirname(__file__), "home", "cache", "snapshot.json")
os.makedirs(os.path.dirname(out), exist_ok=True)
with open(out, "w", encoding="utf-8") as f:
    json.dump(snap, f, ensure_ascii=False, indent=1)
print("wrote", out, len(msgs), "messages")
seed_media()
