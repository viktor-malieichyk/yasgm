#!/usr/bin/env bash
# Fake macOS "machines" for end-to-end tests without touching real saves:
# each is a separate HOME with a fake Steam library holding one made-up game
# (app 999999990, saves in its install folder via a local manifest override)
# and the same local-folder "cloud". Reuses the real manifest cache.
#
#   source scripts/fake-machines.sh
#   mk a; mk b                         # create machines a and b
#   echo v1 > "$(saves a)/slot.sav"    # "play" on a
#   y a sync                           # run yasgm as machine a
#   HOME="$T/b" YASGM_CLOUD_CHECK_SECS=5 "$Y" watch &   # a daemon on b
#
# T defaults to target/fm (short enough for the control socket's path).
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
T="${T:-$repo/target/fm}"
Y="$repo/target/debug/yasgm"
APP=999999990
mk() {
  local h="$T/$1" steam
  steam="$h/Library/Application Support/Steam"
  mkdir -p "$steam/steamapps/common/YasgmTest/Saves" "$steam/userdata/12345" "$h/Library/Application Support/yasgm" "$h/Library/Caches/yasgm" "$T/cloud"
  printf '"AppState"\n{\n\t"appid"\t"%s"\n\t"name"\t"YASGM Test Game"\n\t"installdir"\t"YasgmTest"\n}\n' $APP > "$steam/steamapps/appmanifest_$APP.acf"
  printf '{"provider":{"type":"local","path":"%s"}}' "$T/cloud" > "$h/Library/Application Support/yasgm/config.json"
  printf 'YASGM Test Game:\n  files:\n    <base>/Saves:\n      tags: [save]\n  steam:\n    id: %s\n' $APP > "$h/Library/Application Support/yasgm/ludusavi.yaml"
  ln -sf "$HOME/Library/Caches/yasgm/manifest.yaml" "$h/Library/Caches/yasgm/manifest.yaml"
  cp "$HOME/Library/Caches/yasgm/manifest.etag" "$h/Library/Caches/yasgm/manifest.etag"
}
saves() { echo "$T/$1/Library/Application Support/Steam/steamapps/common/YasgmTest/Saves"; }
y() { local m="$1"; shift; HOME="$T/$m" "$Y" "$@"; }
