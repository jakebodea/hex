#!/bin/sh
# Builds a personal Hex.app from this checkout without the release
# Developer ID: the release binary goes into a copy of an installed Hex.app
# (for its icons, speech libraries, driver, and frameworks) and every nested
# component is re-signed with your own Apple Development identity.
#
#   scripts/build-personal-app.sh            # build target/app/Hex.app
#   scripts/build-personal-app.sh --install  # also replace /Applications/Hex.app
#
# HEX_BASE_APP overrides the bundle to copy; HEX_SIGN_IDENTITY overrides the
# signing identity. In-app updates are turned off on install so the official
# feed never replaces this build.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
base=${HEX_BASE_APP:-/Applications/Hex.app}
. "$root/scripts/personal-signing.sh"
. "$root/scripts/macos-signing.sh"
version=$(hex_version "$root")
build_number=$(hex_build_number "$version")
bundle="$root/target/app/Hex.app"
installed=/Applications/Hex.app

if [ ! -x "$base/Contents/MacOS/hex" ]; then
  echo "Base app $base not found. Install Hex once, or set HEX_BASE_APP." >&2
  exit 1
fi

identity=$(hex_personal_codesign_identity "$base")
base_requirement=$(codesign -d -r- "$base" 2>&1 | sed -n 's/^designated => //p')
if [ -z "$base_requirement" ]; then
  echo "Base app has no designated signing requirement." >&2
  exit 1
fi

export MACOSX_DEPLOYMENT_TARGET=15.0
# Older Rust's debug stripping creates dylibs rejected by macOS 27's loader.
# https://github.com/rust-lang/rust/issues/157750
export CARGO_PROFILE_RELEASE_STRIP=none
cargo build --locked --release --manifest-path "$root/Cargo.toml"

rm -rf "$bundle"
mkdir -p "$(dirname "$bundle")"
/usr/bin/ditto "$base" "$bundle"
cp "$root/target/release/voice-control" "$bundle/Contents/MacOS/hex"

# The base app's Info.plist is kept, but permission descriptions follow the
# source: macOS terminates an app that requests a permission it does not declare.
info="$bundle/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$info"
plutil -replace CFBundleVersion -string "$build_number" "$info"
plutil -convert json -o - "$root/app/Info.plist" |
  /usr/bin/python3 -c 'import json, sys
for key, value in json.load(sys.stdin).items():
    if key.endswith("UsageDescription"):
        print(key + "\t" + value)' |
  while IFS="$(printf '\t')" read -r key value; do
    plutil -replace "$key" -string "$value" "$info"
  done

sign() {
  codesign --force --timestamp=none --options runtime --sign "$identity" "$@"
}
frameworks="$bundle/Contents/Frameworks"
sparkle="$frameworks/Sparkle.framework/Versions/B"
sign "$sparkle/XPCServices/Installer.xpc"
sign --preserve-metadata=entitlements "$sparkle/XPCServices/Downloader.xpc"
sign "$sparkle/Autoupdate"
sign "$sparkle/Updater.app"
sign "$frameworks/Sparkle.framework"
for library in "$frameworks"/*.dylib; do
  sign "$library"
done
if [ -d "$bundle/Contents/Library/HexMicrophone.driver" ]; then
  sign "$bundle/Contents/Library/HexMicrophone.driver"
fi
sign --entitlements "$root/app/VoiceControl.entitlements" "$bundle"
codesign --verify --deep --strict "$bundle"
if [ -z "${HEX_SIGN_IDENTITY:-}" ]; then
  codesign --verify --strict -R "=$base_requirement" "$bundle"
fi
echo "Built $bundle (signed by $identity)"

if [ "${1:-}" = "--install" ]; then
  defaults write com.kitlangton.hex2 DisableUpdates -bool true
  osascript -e 'quit app "Hex"' >/dev/null 2>&1 || true
  pkill -f "$installed/Contents/MacOS/hex" 2>/dev/null || true
  pkill -f "target/debug/voice-control app" 2>/dev/null || true
  sleep 1
  if [ -d "$installed" ]; then
    # Keep the previous app recoverable from the Trash.
    osascript -e "tell application \"Finder\" to delete POSIX file \"$installed\"" >/dev/null
  fi
  /usr/bin/ditto "$bundle" "$installed"
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$installed"
  open "$installed"
  echo "Installed $installed"
fi
