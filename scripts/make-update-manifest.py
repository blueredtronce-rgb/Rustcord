#!/usr/bin/env python3
"""Writes the self-updater's feed, latest.json, for the release files in DIR
(docs/UPDATER_DESIGN.md §3 and §14). Signing is CI's job (openssl, release.yml).

    python3 scripts/make-update-manifest.py DIR [--version X.Y.Z] [--base-url URL] [--require-all] [--out FILE]
    python3 scripts/make-update-manifest.py --self-test

On Windows run it with `python`. Errors: one line on stderr, exit 1. Success prints nothing.
Standard library only.
"""

import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
import urllib.parse

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# Platform key -> file name template (frozen: installed apps expect exactly these, §2).
FILES = {
    "windows-x86_64": "Rustcord-Setup-{v}.exe",
    "linux-x86_64": "rustcord-{v}-linux-x86_64.tar.gz",
    "linux-x86_64-deb": "rustcord_{v}_amd64.deb",
    "macos-universal": "Rustcord-{v}-macos-universal.app.tar.gz",
}
MAX_SIZE = 200 * 1024 * 1024
# semver X.Y.Z without leading zeros, prerelease or build (what the app's semver parse accepts).
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
LOOPBACK = {"127.0.0.1", "::1", "localhost"}


class Error(Exception):
    pass


def cargo_version():
    with open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8") as f:
        m = re.search(r'^version\s*=\s*"([^"]+)"', f.read(), re.M)
    if not m:
        raise Error("no version in Cargo.toml")
    return m.group(1)


def check_base_url(url):
    try:
        u = urllib.parse.urlsplit(url)
        host = u.hostname
    except ValueError as e:
        raise Error(f"--base-url {url!r}: {e}")
    local = u.scheme in ("http", "https") and host in LOOPBACK
    if not (u.scheme == "https" or local) or not host or u.username is not None or u.password is not None:
        raise Error(f"--base-url {url!r} must be https://, or http(s):// on 127.0.0.1, [::1] or localhost")
    if not url.endswith("/") or u.query or u.fragment:
        raise Error(f"--base-url {url!r} must end with /")


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def manifest(folder, version, base_url, require_all):
    if not VERSION.fullmatch(version):
        raise Error(f"version {version!r} is not X.Y.Z")
    check_base_url(base_url)
    platforms = {}
    for key, template in FILES.items():
        name = template.format(v=version)
        path = os.path.join(folder, name)
        if not os.path.isfile(path):
            if require_all:
                raise Error(f"{name} is missing in {folder}")
            continue
        size = os.path.getsize(path)
        if not 1 <= size <= MAX_SIZE:
            raise Error(f"{name} is {size} bytes; the app accepts 1 to {MAX_SIZE}")
        platforms[key] = {"url": base_url + name, "sha256": sha256(path), "size": size}
    if not platforms:
        raise Error(f"none of the release files for {version} are in {folder}")
    return {"schema": 1, "version": version, "platforms": platforms}


def encode(m):
    # Bytes, so Windows never writes CRLF; the signature covers exactly these.
    return (json.dumps(m, indent=2, sort_keys=True) + "\n").encode()


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise Error(message)


def main(argv):
    p = Parser(prog="make-update-manifest.py", add_help=True)
    p.add_argument("dir", nargs="?")
    p.add_argument("--version")
    p.add_argument("--base-url")
    p.add_argument("--require-all", action="store_true")
    p.add_argument("--out")
    p.add_argument("--self-test", action="store_true")
    a = p.parse_args(argv)
    if a.self_test:
        self_test()
        return
    if not a.dir:
        raise Error("usage: make-update-manifest.py DIR [--version X.Y.Z] [--base-url URL] [--require-all] [--out FILE]")
    version = a.version or cargo_version()
    base_url = a.base_url or f"https://github.com/r0sc/Rustcord/releases/download/v{version}/"
    data = encode(manifest(a.dir, version, base_url, a.require_all))
    try:
        with open(a.out or os.path.join(a.dir, "latest.json"), "wb") as f:
            f.write(data)
    except OSError as e:
        raise Error(f"could not write the manifest: {e}")


def self_test():
    def fails(*args):
        try:
            manifest(*args)
        except Error:
            return True
        return False

    with tempfile.TemporaryDirectory() as d:
        content = {}
        for i, (key, template) in enumerate(FILES.items()):
            content[key] = bytes([i]) * (i + 1) * 1000
            with open(os.path.join(d, template.format(v="1.2.3")), "wb") as f:
                f.write(content[key])
        base = "https://github.com/r0sc/Rustcord/releases/download/v1.2.3/"
        m = manifest(d, "1.2.3", base, True)
        assert m["schema"] == 1 and m["version"] == "1.2.3"
        assert sorted(m["platforms"]) == sorted(FILES)
        for key, entry in m["platforms"].items():
            assert entry["sha256"] == hashlib.sha256(content[key]).hexdigest()
            assert entry["size"] == len(content[key])
            assert entry["url"] == base + FILES[key].format(v="1.2.3")
        assert m["platforms"]["windows-x86_64"]["url"].endswith("/v1.2.3/Rustcord-Setup-1.2.3.exe")

        # The file the signature covers: sorted keys, two-space indent, LF, one trailing newline.
        out = os.path.join(d, "feed.json")
        main([d, "--version", "1.2.3", "--require-all", "--out", out])
        with open(out, "rb") as f:
            data = f.read()
        assert data == encode(m) and data.endswith(b"}\n") and b"\r" not in data
        assert data.startswith(b'{\n  "platforms": {\n    "linux-x86_64": {\n      "sha256": ')
        assert json.loads(data) == m

        # Missing files: skipped, unless --require-all; none at all fails.
        os.remove(os.path.join(d, FILES["macos-universal"].format(v="1.2.3")))
        assert fails(d, "1.2.3", base, True)
        assert sorted(manifest(d, "1.2.3", base, False)["platforms"]) == ["linux-x86_64", "linux-x86_64-deb", "windows-x86_64"]
        assert fails(d, "9.9.9", "https://example.com/", False)

        # Loopback E2E feeds.
        for ok in ["http://127.0.0.1:8765/", "https://localhost/feed/", "http://[::1]:9/"]:
            assert manifest(d, "1.2.3", ok, False)["platforms"]["windows-x86_64"]["url"] == ok + "Rustcord-Setup-1.2.3.exe"
        for bad in ["http://example.com/", "https://github.com/x", "ftp://127.0.0.1/", "http://127.0.0.1.evil.com/",
                    "http://user@127.0.0.1/", "https://a@github.com/", "https:///", "https://github.com/?a=/", "github.com/"]:
            assert fails(d, "1.2.3", bad, False), bad
        for bad in ["v1.2.3", "1.2", "1.2.3-beta.1", "1.2.3+b", "01.2.3", "1.2.3 ", "1.2.٣"]:
            assert fails(d, bad, base, False), bad

        # Sizes the app would reject.
        open(os.path.join(d, FILES["windows-x86_64"].format(v="2.0.0")), "wb").close()
        assert fails(d, "2.0.0", base, False)

        # CLI errors end in Error (one line, exit 1 in __main__), not argparse's exit 2.
        for argv in [[], ["--bogus"], [d, "--version", "x"]]:
            try:
                main(argv)
                raise AssertionError(argv)
            except Error:
                pass
    assert VERSION.fullmatch(cargo_version())


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except Error as e:
        print(f"make-update-manifest.py: {e}", file=sys.stderr)
        sys.exit(1)
