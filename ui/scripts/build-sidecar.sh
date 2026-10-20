#!/usr/bin/env bash
# Builds the yasgm CLI in release mode and copies it into src-tauri/binaries
# under Tauri's sidecar naming convention (<name>-<target-triple>), so the
# GUI can bundle and invoke it without linking the core crate as a library
# (see DESIGN.md's "Lib+bin split" note). Runs automatically before
# `tauri dev` and `tauri build` via beforeDevCommand/beforeBuildCommand.
#
# The target is Tauri's (TAURI_ENV_TARGET_TRIPLE, set when `tauri build
# --target …` runs this) and defaults to the host. A Windows target built
# from macOS/Linux goes through cargo-xwin (see scripts/build-windows.sh).
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

host="$(rustc -vV | sed -n 's/^host: //p')"
triple="${TAURI_ENV_TARGET_TRIPLE:-$host}"

exe_name="yasgm"
[[ "$triple" == *windows* ]] && exe_name="yasgm.exe"

if [[ "$triple" == "$host" ]]; then
  cargo build --release --manifest-path "$repo_root/Cargo.toml"
  built="$repo_root/target/release/$exe_name"
elif [[ "$triple" == *windows-msvc ]]; then
  cargo xwin build --release --target "$triple" --manifest-path "$repo_root/Cargo.toml"
  built="$repo_root/target/$triple/release/$exe_name"
else
  cargo build --release --target "$triple" --manifest-path "$repo_root/Cargo.toml"
  built="$repo_root/target/$triple/release/$exe_name"
fi

dest_dir="$repo_root/ui/src-tauri/binaries"
mkdir -p "$dest_dir"
dest="$dest_dir/yasgm-$triple"
[[ "$exe_name" == *.exe ]] && dest="$dest.exe"

# Replace, don't overwrite in place: macOS caches a binary's code signature
# by inode, and a rewritten one gets killed on launch (SIGKILL, exit 137).
rm -f "$dest"
cp "$built" "$dest"
chmod +x "$dest"
echo "sidecar ready: $dest"

# `tauri dev` copies the sidecar next to the dev binary only when the Tauri
# crate itself is rebuilt, so a CLI-only change would keep running the old
# copy. Refresh it here (host target only).
dev_copy="$repo_root/ui/src-tauri/target/debug/$exe_name"
if [[ "$triple" == "$host" && -f "$dev_copy" ]]; then
  rm -f "$dev_copy"
  cp "$built" "$dev_copy"
fi
