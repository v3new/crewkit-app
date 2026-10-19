#!/bin/bash
#
# CrewKit installer.
#
#   /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/v3new/crewkit-app/main/install.sh)"
#
# Downloads the latest CrewKit release from GitHub, installs it into
# /Applications, and clears the download-quarantine flag so it opens without
# a Gatekeeper prompt. The script itself is served from this repository and
# the app from its release page; nothing is fetched from anywhere else.
#
# Overrides (mostly for testing):
#   CREWKIT_INSTALL_DIR   install location            (default: /Applications)
#   CREWKIT_LAUNCH        open the app when done, 1/0 (default: 1)
#
set -euo pipefail

APP_NAME="CrewKit"
REPO="v3new/crewkit-app"
DEST="${CREWKIT_INSTALL_DIR:-/Applications}"
LAUNCH="${CREWKIT_LAUNCH:-1}"
TARBALL_URL="https://github.com/${REPO}/releases/latest/download/${APP_NAME}.app.tar.gz"

info() { printf '\033[1;36m→\033[0m %s\n' "$1"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$1" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$1" >&2; exit 1; }

# --- sanity checks --------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || die "CrewKit is a macOS app; this is not macOS."
if [ "$(uname -m)" != "arm64" ]; then
  warn "CrewKit currently ships for Apple Silicon only (your Mac reports $(uname -m))."
  warn "The app may not run. Continuing anyway."
fi

# /Applications is group-writable for admins on most Macs, but not always.
# Fall back to sudo only if we actually can't write there.
if [ -w "$DEST" ]; then SUDO=""; else SUDO="sudo"; fi
[ -n "$SUDO" ] && warn "Installing to $DEST needs administrator rights; you may be asked for your password."

# --- download -------------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

info "Downloading the latest ${APP_NAME} release…"
curl -fSL --proto '=https' --tlsv1.2 "$TARBALL_URL" -o "$TMP/app.tar.gz" \
  || die "Download failed. Check your connection or grab the .dmg from the releases page."

info "Unpacking…"
tar -xzf "$TMP/app.tar.gz" -C "$TMP" || die "The downloaded archive is corrupt."
[ -d "$TMP/${APP_NAME}.app" ] || die "Unexpected archive layout: ${APP_NAME}.app not found."

# --- install --------------------------------------------------------------
if [ -d "$DEST/${APP_NAME}.app" ]; then
  info "Removing the previous version…"
  $SUDO rm -rf "$DEST/${APP_NAME}.app"
fi
info "Installing to ${DEST}…"
$SUDO mkdir -p "$DEST"
$SUDO mv "$TMP/${APP_NAME}.app" "$DEST/"

# Clear com.apple.quarantine so Gatekeeper doesn't block the first launch.
# This is the same consent macOS asks for behind the "Open Anyway" button —
# applied here up front because the user just chose to run this installer.
info "Clearing the quarantine flag…"
$SUDO xattr -dr com.apple.quarantine "$DEST/${APP_NAME}.app" 2>/dev/null || true

ok "${APP_NAME} installed to $DEST/${APP_NAME}.app"

if [ "$LAUNCH" = "1" ]; then
  info "Launching ${APP_NAME}…"
  open "$DEST/${APP_NAME}.app"
else
  echo "  Launch it from Applications or Spotlight."
fi
