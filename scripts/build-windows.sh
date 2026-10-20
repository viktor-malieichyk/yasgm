#!/usr/bin/env bash
# Cross-builds the Windows installer (NSIS, x64) from macOS or Linux:
# dist/YASGM_<version>_x64-setup.exe (+ .sig, the updater signature).
#
# Needs: rustup with the x86_64-pc-windows-msvc target, cargo-xwin
# (`cargo install --locked cargo-xwin`; fetches the Windows SDK/CRT on first
# use under Microsoft's license), NSIS (`brew install nsis`), llvm
# (`brew install llvm`), Node. For the updater signature, source the signing
# env first: `source ~/.tauri/yasgm-signing.env` (D20).
#
# Tauri calls Windows cross-builds experimental: no Authenticode signing
# (the installer is unsigned, so SmartScreen warns; Windows code signing is
# post-1.0) and untested on hardware beyond what the owner runs by hand.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$(dirname "$(rustup which rustc)"):$HOME/.cargo/bin:/opt/homebrew/opt/llvm/bin:$PATH"
target=x86_64-pc-windows-msvc

for tool in cargo-xwin makensis; do
  command -v "$tool" >/dev/null || { echo "$tool not found (see the header of this script)" >&2; exit 1; }
done
rustup target add "$target" >/dev/null

cd "$repo_root/ui"
[[ -d node_modules ]] || npm ci --no-audit --no-fund
npm run tauri build -- --runner cargo-xwin --target "$target" --bundles nsis

bundle="$repo_root/ui/src-tauri/target/$target/release/bundle/nsis"
mkdir -p "$repo_root/dist"
cp "$bundle"/*_x64-setup.exe "$bundle"/*_x64-setup.exe.sig "$repo_root/dist/"
ls -la "$repo_root"/dist/*_x64-setup.exe
