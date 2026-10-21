# AGENTS.md — orientation for coding agents

Read this first, then `DESIGN.md` (decisions D1–D21, architecture, the v1.0
release plan with per-step status, and every finding so far). `README.md` is
the user-facing install/usage doc.

## What this is

**YASGM** ("Yet another save game manager") syncs and versions PC game saves
across Windows, macOS and SteamOS (Steam Deck) through the user's *own*
cloud: OneDrive (Graph API, app-folder scope `Apps/YASGM/`) or a local
folder. No server, no telemetry. Save locations come from the Ludusavi
Manifest (PCGamingWiki), so there is no per-game setup. Personal project of
the owner (GitHub `viktor-malieichyk/yasgm`), pre-1.0, MIT.

Three shipped pieces share one Rust CLI, `yasgm`:

| Piece | Where | How it uses the CLI |
|---|---|---|
| CLI + daemon (`yasgm watch`) | `src/` | — |
| Desktop app (Windows/macOS), Tauri v2 + vanilla JS | `ui/` | bundled as a sidecar (`ui/src-tauri/binaries/yasgm-<triple>`), spawned per command; the tray daemon is `yasgm watch --tray` |
| Decky Loader plugin (Steam Deck Gaming Mode) | `decky/` | static musl builds `bin/yasgm-x86_64` / `bin/yasgm-aarch64`; `main.py` supervises the daemon and talks to it over its Unix socket |

## Code map (`src/`)

- `main.rs` — module list and command dispatch only.
- `commands/` — the CLI commands: `sync.rs` (`sync`, `backup`, `run`,
  `run_sync`, `SyncEvent`), `setup.rs` (`auth`, `status`, `provider`,
  `autostart`), `saves.rs` (`versions`, `restore`, `pin`/`unpin`/`rm`,
  `cloud-games`, `move-account`, folder lookups), `games.rs` (`doctor`,
  `manifest-gaps`, `config`/`games`, `icons`, `running`, `verify-update`),
  `selftest.rs`.
- `daemon.rs` — `yasgm watch`: watch loop, `GameSession`, `EventLog`,
  shared `DaemonState` (so `status`/`events` answer during a sync), the
  periodic cloud check, reload when games/settings change.
- `ctx.rs` — `Ctx` (manifest + Steam library + account), `build_store`,
  `load_indexes`; `args.rs` — argument helpers.
- `sync.rs` — the per-game sync decision (three-state: local / last-synced /
  cloud → upload, download, conflict-keep-both, cloud-ahead for backup-only).
- `store.rs` — cloud layout `accounts/<steam-account>/games/<appid>/{index.json,versions/*.zip}`,
  retention, pins, pending (offline) store, `import_game` / account moves.
- `provider.rs` (trait, `Send + Sync`), `onedrive.rs` (Graph, PKCE + device
  code, one shared `ureq::Agent`), `local.rs` (folder provider).
- `snapshot.rs` — capture/extract zips with placeholder-relative paths
  (cross-OS restore); `resolve.rs` — placeholder → OS/Proton paths, cross-OS gaps.
- `steam.rs` / `vdf.rs` — libraries, installed games, Steam Cloud detection,
  `current_account` (from `config/loginusers.vdf`, *not* the first `userdata` folder).
- `manifest.rs` — Ludusavi manifest fetch/cache (ETag) + local overrides
  (`<config>/yasgm/ludusavi.yaml`).
- `lock.rs` — `watch.lock` (one daemon per user, fails fast, exit code 3)
  and `sync.lock` (per-operation, *waits*; taken once per process, never
  nested). `control.rs` — daemon Unix socket (`watch send …`, not on Windows).
- `activity.rs` — current sync phase (waiting/checking/uploading/downloading).
- `config.rs` (config.json, state.json, atomic writes via `fsutil.rs`),
  `running.rs` (process detection), `tray.rs`, `gui.rs`, `autostart.rs`, `icons.rs`.

Local files: config/state/tokens in the platform config dir `yasgm/`
(macOS `~/Library/Application Support/yasgm/`), lock/pid/socket in the data
dir `yasgm/`, manifest/icons/cloud-games cache in the cache dir `yasgm/`.

## Build, test, run

```
cargo build && cargo test                  # CLI (64 tests; keep them passing)
cd ui && npm run tauri dev                 # desktop app (builds the sidecar first)
bash ui/scripts/build-sidecar.sh           # refresh the sidecar + target/debug copy after CLI-only changes
scripts/build-linux.sh [x86_64|aarch64]    # static musl CLI (cargo-zigbuild + zig 0.17)
scripts/package-decky.sh                   # → dist/yasgm-decky-<ver>.zip + .sha256
scripts/build-windows.sh                   # NSIS installer cross-built from the Mac
scripts/version.sh [<ver> | --check v<ver>]   # one version across 5 files
```

`.github/workflows/release.yml`: tag `v<ver>` → draft release, macOS
(aarch64 + x86_64) and Windows NSIS via tauri-action with updater
signatures + `latest.json`, Decky zip, then publish. **Written, never run.**

Testing on SteamOS: the `steamac` VM on the dev Mac (`ssh -p 2222
steamos@127.0.0.1`, user `steamos`, aarch64) — see DESIGN.md "Test
environment". It is often powered off; real Gaming Mode behaviour of the
plugin is still unverified.

## Rules (from the owner — follow them)

- **`DESIGN.md` is a living document**: record every decision, finding and
  status change there (dated), in the same commit as the code.
- Commit messages end with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Work on `master`.
  **Push or tag only when the owner asks.**
- **Never read or print signing secrets.** For signed builds only run
  `source ~/.tauri/yasgm-signing.env`. GitHub secrets
  (`TAURI_SIGNING_PRIVATE_KEY`, `…_PASSWORD`) are set by the owner. sudo on
  the VM is run by the owner.
- Non-destructive is a hard rule (D14): never overwrite/delete local saves
  without snapshotting first; conflicts keep both (D5).
- Verify live, not just with unit tests, and say plainly what wasn't
  verified (especially Windows and Gaming Mode). New platform-specific code
  isn't verified until it has run on that platform.
- Match the surrounding code style; comments explain *why*.

## Gotchas learned the hard way

- macOS SIGKILLs (exit 137) a binary overwritten in place — `rm` then `cp`
  (the build script does this).
- `tauri dev` only copies the sidecar when the Tauri crate rebuilds; use
  `build-sidecar.sh` after CLI-only changes, then restart the daemon
  (`yasgm watch stop` / `watch start`).
- Xcode 27's `strip` corrupts proc-macro dylibs → `[profile.release.build-override] strip = false` in both crates.
- flock belongs to the open file: taking `sync.lock` twice in one process
  deadlocks. Child processes forked while a lock fd is open can briefly
  share it (tests retry).
- OneDrive throttles >8 parallel requests (16 → 20–30 s).
- `games --json` parses the 17 MB manifest (~seconds); the plugin backend
  caches games/icons/status/cloud games and answers with `fresh=false`.
- Windows: Steam paths differ in case/slashes between the registry and
  `libraryfolders.vdf` — compare normalized paths.
- Release builds of the desktop updater reject non-https feeds; test the
  updater with a `--debug` build of the *old* version.

## Current state (2026-10-22) and what's next

Done: v1.0 steps 1–5 and 8, plus the 2026-10-21 code review (DESIGN.md
"Code review 2026-10-21": conflict rules, offline backups, per-game errors,
periodic cloud check, conditional index writes, OneDrive retries, desktop
CSP, signed plugin updates, `main.rs` split, clippy clean).

Testing aids: `cargo test` (83 tests, incl. two-machine sync scenarios in
`sync.rs`); `yasgm selftest` (real OneDrive round trip, cleans up);
`scripts/fake-machines.sh` (fake HOMEs + made-up game + local-folder cloud
for end-to-end runs, incl. daemons; `YASGM_CLOUD_CHECK_SECS` shortens the
cloud check).

Open:
1. Owner installs the latest signed `dist/yasgm-decky-0.1.0.zip` on the
   Deck, then run `yasgm move-account 1291627233` (no `--keep-source`) to
   finish moving saves uploaded under the wrong Steam account into
   `57948699` and delete the originals.
2. Step 6 — SteamOS validation in Gaming Mode (toasts, status card, icons,
   progress bar, update/rollback incl. signature check, frozen-game
   recovery, machine name, Proton paths, Windows↔Deck restore, sleep
   mid-sync).
3. Step 7 — first release run (deferred by the owner; needs the secrets).
4. Step 9 — macOS signing + notarization (needs the owner's Apple credentials).
5. Windows verification on hardware (updater install, `taskkill` stop, locks).
6. The tray "Open YASGM" click is untested.

Post-1.0: Flathub GUI on SteamOS, Linux tray, more providers, GOG/Epic,
Windows code signing.
