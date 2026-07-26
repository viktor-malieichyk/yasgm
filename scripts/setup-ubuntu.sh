#!/usr/bin/env bash
# Sets up an Ubuntu machine to build and test YASGM — both the core CLI/
# daemon (src/) and the Tauri v2 GUI (ui/) — and does a full build+test
# pass. Meant for Phase 2's "needs SteamOS/Linux hardware" validation work
# (see DESIGN.md): this won't give you a real SteamOS/Proton environment,
# but it's the closest thing on a generic Ubuntu box, and it's what
# actually builds/lints/tests the Linux code paths that were previously
# only "written, unvalidated".
#
# Usage: run from the repo root (where this script lives at scripts/):
#   bash scripts/setup-ubuntu.sh
#
# Safe to re-run — every install step checks first. Requires sudo for apt
# packages; you'll get the normal sudo password prompt.
#
# What this does NOT do: install/configure Steam, or set up a real Proton
# prefix. `yasgm doctor` will correctly report "Steam installation not
# found" on a bare VM — that's expected. It also doesn't set up Flatpak
# packaging (a separate, still-open Phase 2 item); see the note at the end
# for that.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ ! -f Cargo.toml || ! -d ui ]]; then
  echo "error: run this from a YASGM checkout (expected Cargo.toml and ui/ here: $REPO_ROOT)" >&2
  exit 1
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "error: this script is for Ubuntu/Debian (apt-get not found)" >&2
  exit 1
fi

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$1"; }
ok()  { printf '\033[1;32m  ok:\033[0m %s\n' "$1"; }
fail_flag=0
step_failed() { printf '\033[1;31m  FAILED:\033[0m %s\n' "$1"; fail_flag=1; }

# ---- system packages -------------------------------------------------------

log "Installing system packages (apt)"
sudo apt-get update -y

# build-essential + pkg-config + curl/git: core CLI's build needs a C
# compiler (the `ring` crate, used by rustls for TLS) but no OpenSSL dev
# package — this project uses rustls, not native-tls/openssl, so there's no
# libssl-dev requirement.
#
# The rest (webkit2gtk/gtk3/appindicator/librsvg/libxdo/patchelf) are
# Tauri v2's official Linux prerequisites for the GUI, including the
# tray-icon feature this GUI actually uses (needs libayatana-appindicator).
sudo apt-get install -y \
  build-essential \
  curl \
  git \
  pkg-config \
  file \
  libwebkit2gtk-4.1-dev \
  libgtk-3-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  libxdo-dev \
  patchelf

ok "system packages installed"

# ---- Rust -------------------------------------------------------------------

log "Checking Rust toolchain"
if ! command -v cargo >/dev/null 2>&1; then
  echo "installing rustup..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# shellcheck disable=SC1090
source "$HOME/.cargo/env"
ok "$(rustc --version), $(cargo --version)"

# ---- Node.js (for the GUI's beforeDevCommand/beforeBuildCommand) -----------

log "Checking Node.js"
need_node=1
if command -v node >/dev/null 2>&1; then
  node_major="$(node --version | sed -E 's/^v([0-9]+).*/\1/')"
  if [[ "$node_major" -ge 18 ]]; then
    need_node=0
  fi
fi
if [[ "$need_node" -eq 1 ]]; then
  echo "installing Node.js LTS via NodeSource..."
  curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash -
  sudo apt-get install -y nodejs
fi
ok "$(node --version), npm $(npm --version)"

# ---- build + test the core CLI ---------------------------------------------

log "Building yasgm (release)"
if cargo build --release; then
  ok "cargo build --release"
else
  step_failed "cargo build --release"
fi

log "Running the core test suite"
if cargo test; then
  ok "cargo test (all unit tests are self-contained — no real Steam library needed)"
else
  step_failed "cargo test"
fi

log "Running clippy (informational — not required to pass)"
cargo clippy --all-targets 2>&1 | tail -40

# ---- build the GUI ----------------------------------------------------------

log "Installing GUI npm dependencies"
if (cd ui && npm install); then
  ok "npm install"
else
  step_failed "npm install"
fi

log "Building the GUI (npm run tauri build — produces a .deb/.AppImage)"
if (cd ui && npm run tauri build); then
  ok "GUI build — check ui/src-tauri/target/release/bundle/ for the .deb/.AppImage"
else
  step_failed "GUI build (npm run tauri build)"
fi

# ---- summary ----------------------------------------------------------------

log "Summary"
if [[ "$fail_flag" -eq 0 ]]; then
  echo "Everything built and tested cleanly."
else
  echo "One or more steps failed — scroll up for the exact command and error."
fi
echo
echo "Try it out:"
echo "  ./target/release/yasgm doctor"
echo "    (will report \"Steam installation not found\" on a bare VM — expected;"
echo "     install Steam + a game to get real path-resolution coverage)"
echo "  ls ui/src-tauri/target/release/bundle/"
echo
echo "Still open after this (see DESIGN.md, Phase 2 remaining items):"
echo "  - Proton path mapping against a REAL Proton prefix (this box has no"
echo "    Steam/Proton installed; the mapping *logic* is already unit-tested"
echo "    without needing one — see src/resolve.rs's tests)"
echo "  - Flatpak packaging: not attempted by this script; would need"
echo "    'sudo apt install flatpak flatpak-builder' plus a Flatpak manifest,"
echo "    which doesn't exist in this repo yet"
