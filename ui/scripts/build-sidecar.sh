#!/usr/bin/env bash
# Builds the yasgm CLI in release mode and copies it into src-tauri/binaries
# under Tauri's sidecar naming convention (<name>-<host-triple>), so the GUI
# can bundle and invoke it without linking the core crate as a library (see
# DESIGN.md's "Lib+bin split" note). Runs automatically before `tauri dev`
# and `tauri build` via beforeDevCommand/beforeBuildCommand.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

cargo build --release --manifest-path "$repo_root/Cargo.toml"

triple="$(rustc -vV | sed -n 's/^host: //p')"
exe_name="yasgm"
if [[ "$triple" == *windows* ]]; then
  exe_name="yasgm.exe"
fi

dest_dir="$repo_root/ui/src-tauri/binaries"
mkdir -p "$dest_dir"
dest="$dest_dir/yasgm-$triple"
if [[ "$exe_name" == *.exe ]]; then
  dest="$dest.exe"
fi

cp "$repo_root/target/release/$exe_name" "$dest"
chmod +x "$dest"
echo "sidecar ready: $dest"
