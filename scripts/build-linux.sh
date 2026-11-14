#!/usr/bin/env bash
# Builds the Linux packages from this tree into dist/:
#   rustcord_<version>_<arch>.deb               sudo apt install ./rustcord_<version>_<arch>.deb
#   rustcord-<version>-linux-<machine>.tar.gz   per user, no root: tar xf …; sh rustcord-<version>/install.sh
# Needs Rust stable, build-essential, pkg-config, libasound2-dev, libssl-dev and dpkg-deb.
#   bash scripts/build-linux.sh
set -euo pipefail
# The packages' folders get their modes from it (a 077 umask would make /usr/share/doc/rustcord unreadable).
umask 022

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
cargo build --release --locked --bin rustcord
bin=${CARGO_TARGET_DIR:-target}/release/rustcord
version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
arch=$(dpkg --print-architecture)
# The binary needs at most the build machine's glibc (CI builds on Ubuntu 22.04: 2.35).
glibc=$(getconf GNU_LIBC_VERSION | cut -d' ' -f2)
sizes=(16 32 48 64 128 256 512)
dist=$root/dist
mkdir -p "$dist"
# Staged on a Linux file system: dpkg-deb checks modes that /mnt/c (WSL) can't hold.
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# .deb: system-wide files only; the autostart entry is per user, written by the app (or install.sh).
deb=$stage/deb
install -Dm755 "$bin" "$deb/usr/bin/rustcord"
install -Dm644 assets/linux/rustcord.desktop "$deb/usr/share/applications/rustcord.desktop"
for s in "${sizes[@]}"; do
	install -Dm644 "assets/icons/rustcord-$s.png" "$deb/usr/share/icons/hicolor/${s}x$s/apps/rustcord.png"
done
install -Dm644 LICENSE "$deb/usr/share/doc/rustcord/copyright"
installed_size=$(du -sk "$deb" | cut -f1)
mkdir -m755 "$deb/DEBIAN"
cat >"$deb/DEBIAN/control" <<EOF
Package: rustcord
Version: $version
Architecture: $arch
Maintainer: Rustcord <noreply@rustcord.invalid>
Depends: libc6 (>= $glibc), libstdc++6, libgcc-s1, libasound2t64 | libasound2, libssl3t64 | libssl3, libgl1, libegl1, libx11-6, libx11-xcb1, libxcursor1, libxi6, libxrandr2, libxkbcommon0, libxkbcommon-x11-0, libdbus-1-3
Recommends: fonts-noto-color-emoji, xdg-desktop-portal
Section: net
Priority: optional
Installed-Size: $installed_size
Homepage: https://github.com/r0sc/Rustcord
Description: Lightweight third-party chat client
 Rustcord is an unofficial desktop client written in Rust. It is not
 affiliated with Discord.
EOF
chmod 644 "$deb/DEBIAN/control"
deb_out=$dist/rustcord_${version}_${arch}.deb
# xz: every dpkg reads it (Ubuntu's default zstd needs dpkg 1.21.18+).
dpkg-deb --root-owner-group -Zxz --build "$deb" "$deb_out"

# .tar.gz with the per-user installer.
dir=$stage/rustcord-$version
install -Dm755 "$bin" "$dir/rustcord"
install -Dm755 scripts/linux/install.sh "$dir/install.sh"
install -Dm644 assets/linux/rustcord.desktop "$dir/rustcord.desktop"
install -Dm644 LICENSE "$dir/LICENSE"
for s in "${sizes[@]}"; do
	install -Dm644 "assets/icons/rustcord-$s.png" "$dir/icons/rustcord-$s.png"
done
tar_out=$dist/rustcord-$version-linux-$(uname -m).tar.gz
tar -C "$stage" --owner=0 --group=0 -czf "$tar_out" "rustcord-$version"

ls -lh "$deb_out" "$tar_out"
