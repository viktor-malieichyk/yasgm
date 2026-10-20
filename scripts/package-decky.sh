#!/usr/bin/env bash
# Builds the sideloadable Decky plugin zip (D21): the frontend bundle, the
# Python backend and static yasgm binaries for both SteamOS architectures,
# in Decky's layout:
#   yasgm/{dist/index.js, main.py, plugin.json, package.json, LICENSE,
#          README.md, bin/yasgm-x86_64, bin/yasgm-aarch64}
# Output: dist/yasgm-decky-<version>.zip (+ .sha256). Install it via Decky's
# developer mode ("Install plugin from ZIP file") or unpack into
# ~/homebrew/plugins/.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
plugin_src="$repo_root/decky"
version="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$plugin_src/package.json")"

"$repo_root/scripts/build-linux.sh" x86_64 aarch64

(cd "$plugin_src" && { [[ -d node_modules ]] || npm ci --no-audit --no-fund; } && npm run build)

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
dest="$stage/yasgm"
mkdir -p "$dest/dist" "$dest/bin"
cp "$plugin_src/dist/index.js" "$dest/dist/"
cp "$plugin_src/main.py" "$plugin_src/plugin.json" "$plugin_src/package.json" "$plugin_src/README.md" "$dest/"
cp "$repo_root/LICENSE" "$dest/"
for arch in x86_64 aarch64; do
  cp "$repo_root/dist/yasgm-linux-$arch" "$dest/bin/yasgm-$arch"
  chmod +x "$dest/bin/yasgm-$arch"
done

zip_path="$repo_root/dist/yasgm-decky-$version.zip"
rm -f "$zip_path"
(cd "$stage" && zip -qr -X "$zip_path" yasgm)
(cd "$repo_root/dist" && shasum -a 256 "$(basename "$zip_path")" > "$(basename "$zip_path").sha256")
echo "built $zip_path ($(du -h "$zip_path" | cut -f1))"
