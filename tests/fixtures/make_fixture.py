"""Generates tests/fixtures/home/cache/snapshot.json: fake servers/DMs/messages for offline
screenshots and startup/RAM measurements (RUSTCORD_HOME=tests/fixtures/home RUSTCORD_OFFLINE=1).
All names and ids are invented."""
import json, os, datetime

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
