#!/bin/sh
# Installs Rustcord for this user only (no root), from the unpacked .tar.gz, or removes it.
#   sh install.sh                        install or update; a first install opens Rustcord when
#                                        you log in, an update keeps the choice made in the app
#   sh install.sh --no-autostart         the same, but Rustcord does not open when you log in
#   sh install.sh --update               the self-updater's update (docs/UPDATER_DESIGN.md §12):
#                                        an install that leaves the login entry and the running
#                                        Rustcord alone; fails when Rustcord is not installed
#   sh install.sh --uninstall [--purge]  remove it (--purge: also settings, cache, saved login)
# Files: ~/.local/bin/rustcord, ~/.local/share/applications/rustcord.desktop, hicolor icons
# in ~/.local/share/icons, ~/.config/autostart/rustcord.desktop and a copy of this script in
# ~/.local/share/rustcord/ (for --uninstall later).
# Tests: RUSTCORD_SETUP_TEST=1 puts everything under $RUSTCORD_SETUP_PREFIX (default
# ./test-prefix) instead of $HOME and never reaches the real Rustcord, its autostart entry or
# the saved login.
set -eu

usage="usage: sh install.sh [--no-autostart | --update] | --uninstall [--purge]"
mode=install autostart=1 purge=0
for arg; do
	case $arg in
	--no-autostart) autostart=0 ;;
	--update) mode=update ;;
	--uninstall) mode=uninstall ;;
	--purge) purge=1 ;;
	*) echo "$usage" >&2; exit 2 ;;
	esac
done
src=$(cd "$(dirname "$0")" && pwd)
sizes="16 32 48 64 128 256 512"

if [ "${RUSTCORD_SETUP_TEST:-}" = 1 ]; then
	test=1
	home=${RUSTCORD_SETUP_PREFIX:-$PWD/test-prefix}
	data=$home/.local/share config=$home/.config cache=$home/.cache
	# A data dir of its own: `--quit` can never reach the user's running Rustcord.
	RUSTCORD_HOME=$home/test-home
	export RUSTCORD_HOME
else
	test=0
	home=$HOME
	data=${XDG_DATA_HOME:-$HOME/.local/share}
	config=${XDG_CONFIG_HOME:-$HOME/.config}
	cache=${XDG_CACHE_HOME:-$HOME/.cache}
fi
bin=$home/.local/bin/rustcord
apps=$data/applications
icons=$data/icons/hicolor
login_entry=$config/autostart/rustcord.desktop

# Desktop Entry spec, "Exec key": a quoted argument with \ " ` $ escaped, then the string
# escape (\ doubled), and % doubled.
exec_arg() {
	printf '"%s"' "$(printf '%s' "$1" | sed -e 's/[\\"`$]/\\&/g' -e 's/\\/\\\\/g' -e 's/%/%%/g')"
}
# A plain string value (TryExec): \ doubled.
string_value() {
	printf '%s' "$1" | sed 's/\\/\\\\/g'
}

# Asks a running Rustcord to quit and waits for it.
quit_running() {
	if [ -x "$bin" ]; then "$bin" --quit || true; fi
}

# The login entry (the same file the app's "Open Rustcord when I log in" switch writes);
# Hidden=true is the XDG way to say "don't start", so the app's one-time default leaves it off.
write_login_entry() {
	mkdir -p "$(dirname "$login_entry")"
	{
		printf '[Desktop Entry]\nType=Application\nName=Rustcord\nComment=Start Rustcord when you log in\n'
		printf 'Exec=%s --minimized\nTryExec=%s\n' "$(exec_arg "$bin")" "$(string_value "$bin")"
		printf 'Icon=rustcord\nTerminal=false\n'
		if [ "$1" = 1 ]; then printf 'X-GNOME-Autostart-enabled=true\n'; else printf 'Hidden=true\n'; fi
	} >"$login_entry"
}

# Only an icon cache that already exists is refreshed: a new one would be left behind and go stale.
refresh_icons() {
	if [ -f "$icons/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
		gtk-update-icon-cache -q -t -f "$icons" || true
	fi
}

install_app() {
	if [ ! -f "$src/rustcord" ]; then
		echo "install.sh: no rustcord program next to this script. Run the install.sh from the unpacked download." >&2
		exit 1
	fi
	if [ "$mode" = update ]; then
		# The app that runs this quits itself afterwards.
		if [ ! -e "$bin" ]; then
			echo "install.sh: Rustcord is not installed in $bin, so there is nothing to update." >&2
			exit 1
		fi
	else
		quit_running
	fi
	# After the first install the app's "Open Rustcord when I log in" switch owns the login entry.
	fresh=1
	if [ -e "$bin" ]; then fresh=0; fi
	mkdir -p "$(dirname "$bin")" "$apps" "$data/rustcord"
	# Copy, then rename: a copy that still runs keeps its old file.
	cp "$src/rustcord" "$bin.new"
	chmod 755 "$bin.new"
	mv -f "$bin.new" "$bin"
	{
		grep -v -e '^Exec=' -e '^TryExec=' "$src/rustcord.desktop"
		# Absolute: the session's PATH may not include ~/.local/bin.
		printf 'Exec=%s\nTryExec=%s\n' "$(exec_arg "$bin")" "$(string_value "$bin")"
	} >"$apps/rustcord.desktop"
	for s in $sizes; do
		mkdir -p "$icons/${s}x$s/apps"
		cp "$src/icons/rustcord-$s.png" "$icons/${s}x$s/apps/rustcord.png"
	done
	if [ "$src" != "$(cd "$data/rustcord" && pwd)" ]; then
		cp "$src/install.sh" "$data/rustcord/install.sh"
	fi
	if [ "$mode" = install ] && { [ "$fresh" = 1 ] || [ "$autostart" = 0 ]; }; then write_login_entry "$autostart"; fi
	refresh_icons

	# winit loads libxkbcommon-x11 at run time and panics without it.
	if ! PATH="$PATH:/sbin:/usr/sbin" ldconfig -p 2>/dev/null | grep -q libxkbcommon-x11; then
		echo "Rustcord also needs libxkbcommon-x11: install libxkbcommon-x11-0 (Debian, Ubuntu) or libxkbcommon-x11 (Fedora, Arch)."
	fi
	echo "Rustcord is installed. Open it from your app menu."
}

uninstall_app() {
	quit_running
	if [ "$purge" = 1 ]; then
		# The saved login lives in the system keyring, shared with the real Rustcord: tests skip it.
		# Older builds without --clear-login would start the app instead.
		if [ "$test" = 0 ] && [ -x "$bin" ] && grep -q -e '--clear-login' "$bin"; then
			"$bin" --clear-login || echo "install.sh: the saved login could not be removed." >&2
		fi
		rm -rf "$config/rustcord" "$cache/rustcord"
	fi
	rm -f "$bin" "$bin.new" "$apps/rustcord.desktop" "$login_entry"
	for s in $sizes; do
		rm -f "$icons/${s}x$s/apps/rustcord.png"
		rmdir "$icons/${s}x$s/apps" "$icons/${s}x$s" 2>/dev/null || true
	done
	refresh_icons
	rm -rf "$data/rustcord"
	if [ "$test" = 1 ]; then
		rm -rf "$home/test-home"
		# The folders a test install created, if empty now (rmdir never removes anything else).
		for d in "$apps" "$icons" "$data/icons" "$data" "$home/.local/bin" "$home/.local" "$config/autostart" "$config" "$cache" "$home"; do
			rmdir "$d" 2>/dev/null || true
		done
	fi
	echo "Rustcord is uninstalled."
}

if [ "$mode" = uninstall ]; then uninstall_app; else install_app; fi
