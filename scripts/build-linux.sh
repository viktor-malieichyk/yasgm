#!/usr/bin/env bash
# Builds static Linux binaries of the yasgm CLI for SteamOS / the Decky
# plugin: x86_64 (Steam Deck) and aarch64 (ARM SteamOS, e.g. the steamac
# VM), as dist/yasgm-linux-<arch>. Works from macOS or Linux.
#
# Needs: rustup, zig, cargo-zigbuild (`brew install zig`,
# `cargo install cargo-zigbuild`). Plain `cargo build --target …-musl`
# can't link ring's C code without a cross C compiler; zig provides one.
#
# Usage: scripts/build-linux.sh [x86_64|aarch64 ...]   (default: both)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
arches=("$@")
[[ ${#arches[@]} -eq 0 ]] && arches=(x86_64 aarch64)

command -v rustup >/dev/null || { echo "rustup not found" >&2; exit 1; }
command -v zig >/dev/null || { echo "zig not found (brew install zig)" >&2; exit 1; }
command -v cargo-zigbuild >/dev/null || [[ -x "$HOME/.cargo/bin/cargo-zigbuild" ]] \
  || { echo "cargo-zigbuild not found (cargo install cargo-zigbuild)" >&2; exit 1; }

# A Homebrew/distro rustc earlier on PATH has no musl std; use rustup's.
toolchain_bin="$(dirname "$(rustup which rustc)")"
export PATH="$toolchain_bin:$HOME/.cargo/bin:$PATH"

mkdir -p "$repo_root/dist"
for arch in "${arches[@]}"; do
  case "$arch" in
    x86_64|aarch64) ;;
    *) echo "unknown arch: $arch (x86_64 or aarch64)" >&2; exit 1 ;;
  esac
  target="$arch-unknown-linux-musl"
  rustup target add "$target" >/dev/null
  cargo zigbuild --release --target "$target" --manifest-path "$repo_root/Cargo.toml"
  out="$repo_root/dist/yasgm-linux-$arch"
  cp "$repo_root/target/$target/release/yasgm" "$out"
  chmod +x "$out"
  echo "built $out ($(du -h "$out" | cut -f1))"
done
