#!/usr/bin/env bash
# One version for every artifact: the CLI crate, the Tauri app (crate,
# tauri.conf.json, package.json) and the Decky plugin (package.json).
#
#   scripts/version.sh              print the current version (fails if the
#                                   files disagree)
#   scripts/version.sh 0.2.0        set it everywhere (and refresh Cargo.lock)
#   scripts/version.sh --check v0.2.0
#                                   CI: every file must say 0.2.0
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

python3 - "$repo_root" "$@" <<'EOF'
import json, re, sys
from pathlib import Path

root = Path(sys.argv[1])
args = sys.argv[2:]
CARGO = [root / "Cargo.toml", root / "ui/src-tauri/Cargo.toml"]
JSON = [root / "ui/src-tauri/tauri.conf.json", root / "ui/package.json", root / "decky/package.json"]
# The first `version = "…"` is the [package] one in both crates.
CARGO_RE = re.compile(r'^version\s*=\s*"([^"]+)"', re.M)


def current() -> dict:
    found = {str(p.relative_to(root)): CARGO_RE.search(p.read_text()).group(1) for p in CARGO}
    found |= {str(p.relative_to(root)): json.loads(p.read_text())["version"] for p in JSON}
    return found


def agreed(found: dict) -> str:
    if len(set(found.values())) != 1:
        sys.exit("versions disagree:\n" + "\n".join(f"  {v:10} {f}" for f, v in found.items()))
    return next(iter(found.values()))


if not args:
    print(agreed(current()))
elif args[0] == "--check":
    want = args[1].removeprefix("v")
    have = agreed(current())
    if have != want:
        sys.exit(f"tag {args[1]} doesn't match the version in the files ({have}); run scripts/version.sh {want}")
    print(f"version {have} ok")
else:
    new = args[0].removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", new):
        sys.exit(f"not a version: {new}")
    for p in CARGO:
        p.write_text(CARGO_RE.sub(f'version = "{new}"', p.read_text(), count=1))
    for p in JSON:
        # Edit the text, not a re-dump, to keep each file's formatting.
        text = p.read_text()
        p.write_text(re.sub(r'("version"\s*:\s*")[^"]+(")', rf"\g<1>{new}\g<2>", text, count=1))
    print(f"version set to {agreed(current())}")
EOF

# The lockfiles record each package's own version too; refresh them.
if [[ $# -eq 1 && "$1" != --check ]]; then
  cargo update --workspace --offline --manifest-path "$repo_root/Cargo.toml" >/dev/null 2>&1 || true
  cargo update --workspace --offline --manifest-path "$repo_root/ui/src-tauri/Cargo.toml" >/dev/null 2>&1 || true
  for dir in ui decky; do
    (cd "$repo_root/$dir" && npm install --package-lock-only --no-audit --no-fund >/dev/null 2>&1) || true
  done
fi
