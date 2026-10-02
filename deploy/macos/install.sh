#!/bin/sh
# RogueIM installer for macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/cyberhirsch/RogueIM/main/deploy/macos/install.sh | sh
#
# Downloads the newest release with curl (files fetched this way carry no
# quarantine flag, so macOS shows no "unidentified developer" warning),
# checks it against the release's SHA256SUMS, and copies RogueIM.app to
# /Applications (or ~/Applications if that is not writable).
set -eu

REPO="cyberhirsch/RogueIM"
DMG="RogueIM-macos-universal.dmg"

say() { printf 'RogueIM: %s\n' "$*"; }
fail() { say "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "this installer is for macOS"

tmp=$(mktemp -d)
mnt="$tmp/mnt"
cleanup() {
    hdiutil detach "$mnt" -quiet 2>/dev/null || true
    rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

say "looking up the newest release…"
json=$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=10")
url() { printf '%s' "$json" | grep -o "\"browser_download_url\": *\"[^\"]*/$1\"" | head -n 1 | sed 's/.*"\(https[^"]*\)"/\1/'; }
dmg_url=$(url "$DMG")
sums_url=$(url "SHA256SUMS")
[ -n "$dmg_url" ] && [ -n "$sums_url" ] || fail "no macOS release found"
tag=$(printf '%s' "$dmg_url" | sed 's|.*/download/\([^/]*\)/.*|\1|')

say "downloading $tag…"
curl -fL --progress-bar -o "$tmp/$DMG" "$dmg_url"
curl -fsSL -o "$tmp/SHA256SUMS" "$sums_url"

want=$(grep " \*\{0,1\}$DMG\$" "$tmp/SHA256SUMS" | cut -d ' ' -f 1)
have=$(shasum -a 256 "$tmp/$DMG" | cut -d ' ' -f 1)
[ -n "$want" ] && [ "$want" = "$have" ] || fail "the download does not match its checksum; nothing was installed"

mkdir -p "$mnt"
hdiutil attach -nobrowse -readonly -quiet -mountpoint "$mnt" "$tmp/$DMG" || fail "could not open the disk image"

dest="/Applications"
[ -w "$dest" ] || { dest="$HOME/Applications"; mkdir -p "$dest"; }

if pgrep -x rogueim >/dev/null 2>&1; then
    say "closing the running RogueIM…"
    osascript -e 'tell application "RogueIM" to quit' >/dev/null 2>&1 || pkill -x rogueim || true
    sleep 2
fi

rm -rf "$dest/RogueIM.app"
ditto "$mnt/RogueIM.app" "$dest/RogueIM.app"
say "installed $tag to $dest/RogueIM.app"
open "$dest/RogueIM.app"
