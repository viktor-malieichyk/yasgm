# YASGM (Yet Another Save Game Manager) — Design Document

> **Living document.** Updated whenever a decision is made or the plan changes.
> Last updated: 2026-07-20

## Vision

A setup-free tool that seamlessly syncs PC game saves across devices using the
user's own cloud storage. The only setup step is authorizing a cloud service.
Game save locations are discovered automatically — no per-game configuration.

Target platforms: **Windows 10+**, **SteamOS (Steam Deck)**, **macOS** (native
Steam games only; no Proton/CrossOver on Mac in scope).

## Decisions log

| # | Topic | Decision |
|---|-------|----------|
| D1 | Data scope | **Saves only** (no config/settings files). |
| D2 | Steam Cloud games | Not synced. Instead they get **backup-only mode**: versioned snapshots taken automatically, **restore is manual only**, backup count configurable. |
| D3 | Launchers | **Steam first**; library-provider abstraction so GOG/Epic/Heroic can be added later. |
| D4 | Multi-account | Cloud namespace is **per Steam account ID** (two users on one machine never collide). |
| D5 | Conflicts | **Keep both.** Latest becomes active; the other is preserved as a pinned version until the user deletes it manually (exempt from retention pruning). |
| D6 | Language | **Rust.** |
| D7 | Save-location data | Consume the **Ludusavi Manifest** (compiled from PCGamingWiki); do not scrape PCGW ourselves. Support `.ludusavi.yaml` secondary manifests later. |
| D8 | Cloud providers | Modular provider trait. **OneDrive first** (native Microsoft Graph client, `Files.ReadWrite.AppFolder` scope, approot folder). No rclone dependency. |
| D9 | Retention | Keep **last 10 versions** per game by default; **per-game override**; **warn when a game's stored data exceeds 1 GB**. Conflict-preserved and manually pinned versions are never auto-pruned. |
| D10 | License | **MIT.** Attribute PCGamingWiki (manifest data derives from CC BY-NC-SA wiki content; data is fetched at runtime, not bundled). |
| D11 | Daemon autostart | **Opt-in** (toggle offered after first successful cloud auth). |
| D12 | Minimum OS | Windows 10+; current SteamOS; macOS 13+ (tentative). |
| D13 | Data location | User chooses where local data (staging/backups) lives. See [Local data location](#local-data-location) for cloud-folder caveats. State DB always stays outside cloud-synced folders. |
| D14 | Non-destructive | Hard rule: never delete/overwrite a local file without first capturing it in a snapshot. Deletes propagate as tombstones only. |
| D15 | Per-game control | `mode: sync | backup-only | off` per game. Default: `sync`; auto-downgraded to `backup-only` when Steam Cloud is detected. |
| D16 | Apple Developer signing | Deferred — ship unsigned macOS builds initially. |
| D17 | Name | **"Yet another save game manager" (YASGM)** (decided 2026-07-17, replacing earlier *Bonfire* pick). Binary `yasgm`, OneDrive folder `Apps/YASGM/`, wrapper `yasgm run -- %command%`. Availability checked 2026-07-17: no crates.io crate, no GitHub project with the name — fully free. |

## Open questions

- **Azure app registration** — create under the owner's Microsoft account
  before Phase 1 OneDrive work (free, no subscription; produces the client ID
  embedded in the app). Display name: **YASGM** (shows on consent screen and
  names the OneDrive `Apps/YASGM/` folder).
## Research summary (2026-07-17)

### Existing tools and how they solved configuration

- **Ludusavi** (Rust, Steam Deck support): state of the art for *discovery*.
  Uses the Ludusavi Manifest (19k+ games scraped from PCGamingWiki +
  Steam API), auto-detects Steam/GOG/Epic/Heroic/Lutris libraries, resolves
  Proton prefixes. Cloud support shells out to user-configured rclone —
  backup-oriented, not seamless sync.
- **OpenCloudSaves** (Go): closest product vision (bi-directional cloud sync,
  bundled rclone, Flathub for Deck) but uses hand-curated save definitions —
  the setup burden we want to eliminate.
- **GameSave Manager**: Windows-only, closed source, centrally maintained DB.
- **decky-cloud-save**: Deck plugin over rclone, manual per-game paths.
- **Steam Cloud**: zero-config baseline, but developer opt-in only. Our value
  is covering the rest — and not fighting Valve's sync where it exists.

Pattern: everyone converged on PCGamingWiki as ground truth; winners automate
ingestion.

### PCGamingWiki feasibility

- PCGW Cargo API exposes metadata (Steam AppID, cloud-sync booleans) but **not
  save paths** — those live in `{{Game data/saves}}` wikitext templates, so
  direct extraction means parsing wikitext via the MediaWiki API. Doable but
  a permanent maintenance burden; PCGW asks scrapers to be gentle.
- **Ludusavi Manifest already does this**: single YAML
  (`https://raw.githubusercontent.com/mtkennerly/ludusavi-manifest/master/data/manifest.yaml`),
  ETag-based update checks, placeholders (`<winAppData>`, `<winDocuments>`,
  `<xdgData>`, `<home>`, `<storeUserId>`, …) with per-OS/store `when:`
  constraints. Repo is MIT; wiki content CC BY-NC-SA — fine for MIT
  open-source with attribution since we fetch at runtime.
- Coverage: excellent for popular titles; treat paths as "probable", never as
  authority to delete anything (see D14).

## Architecture

### Components

```
┌─ CLI / GUI / tray ────────────────────────────────┐
├─ Sync engine (versioning, conflicts, retention)   │
├─ Game discovery: Library provider trait (Steam)   │
├─ Path resolver (placeholders → OS/Proton paths)   │
├─ Detection: FS watch + running-game + wrapper     │
├─ Manifest service (fetch, cache, ETag)            │
├─ Cloud provider trait (OneDrive first)            │
└─ State DB (rusqlite) + local staging              │
```

### Cloud provider trait

Minimal surface: `authorize()`, `list(prefix)`, `download(path)`,
`upload(path, bytes)`, `delete(path)`, change-token/etag support. Everything
above it is provider-agnostic.

**OneDrive**: Microsoft Graph, `Files.ReadWrite.AppFolder` scope →
app-sandboxed `Apps/<AppName>/` folder via `/me/drive/special/approot`.
Auth: OAuth2 PKCE with localhost redirect (desktop) + **device-code flow**
fallback (Steam Deck Gaming Mode: enter code on phone). Public client, no
secret; client ID from a free Entra ID (Azure) app registration owned by the
project owner. Registration's display name = consent-screen name = OneDrive
folder name, so it couples to the project name.

A **LocalFolder provider** (user points at any directory, e.g. a
Syncthing-synced or cloud-client-synced folder) is cheap to add and covers
power users — see caveats under [Local data location](#local-data-location).

### Cloud data layout (versioned, per Steam account)

```
approot/
  accounts/<steam-account-id>/
    games/<steam-appid>/
      index.json                      # versions, hashes, machine, OS, tombstones, pins
      versions/
        2026-07-17T21-04-11Z_deck1.zip
        2026-07-16T09-30-02Z_pc.zip
```

- One zip per snapshot: atomic, restore-friendly, low Graph request count.
  Files stored inside zips under **placeholder-relative paths** (e.g.
  `winAppData/Studio/Game/slot1.sav`) so a Windows snapshot restores into a
  Proton prefix on SteamOS and vice versa.
- Snapshot skipped when content hashes are unchanged.
- Retention per D9; conflict/pinned versions exempt (D5).

### Path resolution

- Windows: `<winAppData>` → `%APPDATA%`, etc. (Known Folder API).
- SteamOS/Linux native: `<xdgData>`, `<home>` directly.
- SteamOS Proton: Windows placeholders map into
  `steamapps/compatdata/<AppID>/pfx/drive_c/users/steamuser/...`.
- macOS: `<home>/Library/Application Support/...` (manifest `mac` constraints).
- Steam library enumeration: `libraryfolders.vdf` + `appmanifest_*.acf` →
  installed AppIDs joined against manifest Steam IDs.

### Local manifest overrides

D7: the community Ludusavi Manifest (52k+ games from PCGamingWiki) is ground
truth but not complete or always accurate — e.g. Divinity: Original Sin 2 has
a native Mac build but the manifest only lists a Windows save path (Phase 0
finding). A local file at the platform config dir's `yasgm/ludusavi.yaml`
(same schema as the community manifest: game name → `files`/`steam`/`cloud`)
lets a user patch this themselves instead of waiting on a PCGamingWiki edit
to land upstream:

- **New game name** → inserted outright, and joins the normal Steam-AppID
  merge pool (so it behaves exactly like any other manifest entry from that
  point on).
- **Existing game name** → deep-merged into the community entry: `files`
  unioned per template (the override's rule wins if the exact same template
  string collides), `steam`/`cloud` replaced only if the override sets them.
  This can *add or fix* a path but can't *remove* a bad one from the
  community manifest — acceptable for the gaps seen so far, but a known
  limitation.

Absent by default (not an error); `doctor` hints at the file's path when an
installed game isn't in the manifest at all.

### Steam Cloud detection (→ backup-only mode, D2/D15)

1. `appinfo.vdf` `ufs` section (developer-declared cloud save config).
2. Fallback heuristic: `userdata/<accountid>/<appid>/remotecache.vdf` exists.
3. Cross-check: PCGW Cargo cloud booleans.
User can force any mode per game.

### Game start/stop detection (layered)

1. **FS watching (backbone)**: `notify` crate on resolved save dirs
   (ReadDirectoryChangesW / inotify / FSEvents), debounced until writes settle.
   Catches every launch method, zero setup.
2. **Steam running-app polling**: Windows `HKCU\Software\Valve\Steam\RunningAppID`;
   Linux `~/.steam/registry.vdf` `RunningAppID`. Gates uploads (prefer
   after-exit) and triggers down-sync checks. Known unreliable per-game →
   layers 3/4.
3. **Process watching**: `appmanifest` install dir → running executables
   (`sysinfo`).
4. **Launch wrapper (opt-in per game)**: `yasgm run -- %command%` in Steam
   launch options — guaranteed down-sync before launch, up-sync after exit.
   Never auto-edit `localconfig.vdf` (Steam overwrites it).

Default flow: daemon down-syncs at startup and periodically while idle; FS
watcher detects save changes; upload after confirmed exit or settle timeout.
Mid-session snapshots are safe (additive versions, never overwrites).

### Sync & conflict semantics

- Three-state comparison (local vs last-synced vs cloud) via state DB.
- Newest snapshot wins as active; true conflicts produce two versions, loser
  pinned until manual deletion (D5), with a notification.
- Restore always snapshots current local state first → restore is reversible.
- Dry-run mode; human-readable sync log.

### Local data location

User-configurable (D13). If the user places the data/backup folder inside a
cloud-client-synced folder (OneDrive/Dropbox/Google Drive desktop client,
Syncthing):

- **Double-sync hazard**: don't point the API-based provider *and* a desktop
  sync client at overlapping data — duplication and conflict churn.
- **No completion signal**: the tool can't know when the desktop client has
  finished uploading (e.g. before shutdown).
- **Partial reads**: client may sync a file mid-write → always write
  `tmp + atomic rename`; single-zip snapshots make torn reads near-impossible.
- **Files On-Demand placeholders**: reads may stall on download; no official
  OneDrive client on SteamOS at all.
- **Conflict copies**: desktop clients create `file (conflicted copy)` litter;
  treat the folder as eventually-consistent, ignore foreign files.
- **State DB (SQLite) must never live in a cloud-synced folder** — corruption
  risk. Keep it in the platform app-data dir unconditionally.

This is also exactly the design of the LocalFolder provider, which turns these
caveats into a supported feature.

## Auto-update

- **SteamOS**: Flathub only — Flatpak handles updates; the app must *not*
  self-update inside the sandbox.
- **Windows** (and macOS later): GitHub Releases + the `self_update` crate;
  check on start (opt-out), notify + one-click apply, verify release signatures
  (minisign/ed25519), atomic binary swap. Also publish to winget/scoop
  (and Homebrew later) for users who prefer package managers.

## UI plan

Phased:

1. **CLI** (Phase 1): `sync`, `restore`, `versions`, `games` (list/enable/
   disable/mode), `auth`, `status`, `doctor` (path-resolution report).
2. **Tray/menu-bar** (Phase 3): `tray-icon` + native notifications; status,
   pause, "sync now".
3. **Management GUI** (Phase 4): version browser/restore, per-game settings.
   **Decided: Tauri v2** (webview + Rust backend) — validated 2026-07-20 via
   a minimal shell (`ui/`, see Phase 4 log) that opens a real window and
   round-trips real cloud version data through IPC. Other options that were
   considered: **iced** (pure Rust; what Ludusavi uses) — no webview, more
   UI labor; **egui** — quickest to build, utilitarian look; local web UI
   served by the daemon — works in Deck desktop browser, no extra window
   toolkit.
4. **Decky plugin** (v2, later): Gaming Mode status/controls on Steam Deck.

## Rust stack

`tokio`, `reqwest` (Graph), `oauth2` (PKCE + device code), `serde_yaml`
(manifest), `keyvalues-parser` + binary-VDF parsing (`appinfo.vdf`), `notify`,
`sysinfo`, `rusqlite`, `zip`, `self_update`, `tray-icon`. GUI: Tauri v2 or
iced (open question).

## Phases & estimates (~10–12 weeks total)

- **Phase 0 — Spike: DONE 2026-07-19.** `yasgm doctor` (src/: `manifest.rs`,
  `steam.rs`, `resolve.rs`, `vdf.rs`) downloads the manifest with ETag caching,
  enumerates Steam libraries, resolves save paths per OS (incl. Proton mapping
  code for Linux), detects Steam Cloud, and prints a per-game mode report.
  Validated on macOS against a real library. Findings:
  - Manifest is bigger than advertised: **16.6 MB, 52,822 games, 48,591 with
    Steam IDs** (README says 19k+). serde_yaml parses it in ~1 s.
  - **Manifest carries `cloud.steam` flags** — Steam Cloud detection works from
    manifest + local `remotecache.vdf` heuristic; binary `appinfo.vdf` parsing
    is likely unnecessary (demoted from Phase 1 TODO to optional).
  - Data-quality confirmation: Divinity: Original Sin 2 has a native Mac build
    but the manifest lists only a Windows save path → macOS-native coverage
    will lag Windows. Windows+Proton covers the same rule on Deck; for Mac,
    contribute missing paths to PCGamingWiki as encountered.
  - Rust installed on dev machine via Homebrew rustup
    (`/opt/homebrew/opt/rustup/bin` must be on PATH).
- **Phase 1 — Core engine (~3 wks)**: DONE 2026-07-20.
  - `yasgm auth` (PKCE + localhost loopback, `--device` fallback, token cache
    with refresh, 0600) and `yasgm status` — verified live against a real
    OneDrive; `Apps/YASGM/` folder created.
  - **Snapshot store**: `snapshot.rs` (zip capture/extract; entries grouped
    under per-root "mounts" recording the path template + captured wildcard
    value, so zips restore cross-OS; `@file` sentinel for single-file roots;
    deterministic content hash for unchanged-detection), `store.rs` (cloud
    layout `accounts/<acct>/games/<appid>/{index.json,versions/*.zip}`,
    retention keeping newest N non-pinned, 1 GB warning), Graph file ops in
    `onedrive.rs` (download via pre-authorized URL, simple + chunked
    resumable uploads, delete).
  - Commands: `backup [appid] [--dry-run]`, `versions [appid]`,
    `restore <appid> [--version <id>] [--dry-run]` (restore first preserves
    current local state as a new version — D14), hidden `selftest` doing a
    full cloud roundtrip (capture→upload→dedupe→mutate→upload→delete local→
    restore→verify→cleanup) — **passed against real OneDrive**, and real
    DOS2 Definitive Edition saves backed up from macOS.
  - **Finding/fix**: multiple manifest entries can share one Steam AppID
    (DOS2 base + Definitive Edition are both 435150). Entries are now merged
    (union of file rules, OR of cloud flags) deterministically; before the
    fix, HashMap ordering made doctor/backup pick different entries.
  - **Sync engine** (`sync.rs`): three-state comparison (local capture vs
    per-machine state vs cloud head). The **active head is the newest
    non-pinned version**; pinned versions are preserved archives that never
    compete for "latest" — this is what makes safety-preservation and D5
    conflict-keeping immune to download/restore ping-pong loops. Conflict
    flow: pin old head, upload local as new head, tell the user the `rm`
    command for manual deletion (D5). Restore re-publishes restored content
    as the new head so sync doesn't undo it. Backup-only games upload
    normally but only report (never auto-download) a newer cloud head.
  - **Per-game config** (`config.json`): `mode: auto|sync|backup|off` +
    `keep` count — `yasgm config` command (D15, D9). Per-machine sync state
    in `state.json` (deviation from plan: JSON instead of rusqlite — enough
    until file-level tracking arrives with the daemon).
  - Commands added: `sync [appid] [--dry-run]`, `config`, `pin`, `unpin`,
    `rm`. Selftest extended with a sync-matrix section (InSync → Uploaded →
    Conflict-with-pinning → Downloaded) — **passes against real OneDrive**;
    real-library `sync` runs correctly (DOS2 in sync; repeat runs stable).
  - **Windows validation: DONE 2026-07-19.** `doctor`, `auth --device`, and
    `selftest` (full 7-step cloud roundtrip incl. sync-matrix) verified
    against a real OneDrive account and real Steam library on Windows
    (aarch64). Build requires MSVC Build Tools (`link.exe`) **and** LLVM
    `clang` (the `ring` crate needs it to assemble on aarch64-pc-windows-msvc
    — not installed by the VC++ workload alone). Two auth bugs found and
    fixed:
    - `login_interactive`'s and `login_device`'s token-exchange requests
      were missing the `scope` parameter (only `refresh()` had it) —
      `/consumers` rejects the exchange with `AADSTS900144`.
    - `open_browser`'s Windows branch shelled out to `cmd /C start "" <url>`;
      cmd.exe's own parser splits unquoted `&` (ubiquitous in OAuth query
      strings) into separate commands, and quoting the target still fought
      `start`'s title/target argument ambiguity. Fixed by invoking
      `explorer.exe <url>` directly, skipping cmd.exe's parser entirely.
    - Separately observed (not a bug, environment-specific): in a sandboxed
      command-execution context, a spawned browser can open in a session
      invisible to the user — `auth --device` sidesteps this since it needs
      no spawned browser at all, only a URL+code the user enters manually.
  - **Quota-error handling: DONE 2026-07-20.** Graph returns 507 Insufficient
    Storage on quota exhaustion (the AppFolder scope can't read `/me/drive`
    for a proactive check — see Azure appendix). `onedrive::upload` now
    special-cases 507 across all three request sites (simple PUT, chunked
    session creation, chunked PUT) into a distinct `QuotaExceeded` marker
    error instead of a raw Graph JSON dump. `backup`/`sync`'s per-game loops
    downcast for it and stop the batch with one clear, actionable message
    instead of aborting via `?` mid-loop — which previously also skipped
    `state.save()`, silently losing progress already made earlier in that
    run. **Phase 1 complete.**
- **Phase 2 — SteamOS + macOS resolution (~2–2.5 wks)**: IN PROGRESS.
  - **Device-code auth polish: DONE 2026-07-20** (the only slice testable
    from a Windows machine with no SteamOS/macOS/Linux hardware available —
    the rest of this phase needs that hardware to validate against).
    `login_device` now captures `expires_in` (defaulting to Entra's 900s if
    the server omits it) as a local backstop alongside the server's own
    `expired_token`, gives friendly terminal messages for expiry and
    `authorization_declined` instead of a raw OAuth error code, and prints a
    "still waiting…" ping every 30s so a long silent wait (switching devices
    to sign in) doesn't look hung. Verified live: real device-code sign-in
    on Windows showed pings at 30/60/90/120/150/180s then succeeded.
  - **Wildcard-capture bugfix: DONE 2026-07-20** (found in a code review of
    the cross-OS mount-resolution path; not Proton/macOS-specific, so
    testable on Windows). `snapshot.rs`'s `wildcard_value` recorded the
    *whole* matched path segment for a `*`-wildcard placeholder (e.g.
    `<storeUserId>`), not just the substituted portion. That's correct only
    when the wildcard fills a whole segment on its own; for a template that
    embeds it inline (e.g. `Slot<storeUserId>Data`), it would capture the
    entire segment, and restoring later would then do
    `"Slot*Data".replace('*', "Slot76561198012345678Data")`, doubling the
    literal text into a corrupted path instead of reconstructing the
    original. Fixed to extract only the substring between the wildcard's
    prefix/suffix. Added unit tests (`cargo test`) covering the full-segment
    case, the embedded case, the capture→restore round-trip, and two
    None-cases; `selftest`'s cloud roundtrip still passes.
  - Code review of `resolve.rs`'s Proton and macOS placeholder logic
    (`constraint_matches`, `applies_on`, `windows_only`, `native_vars`,
    `proton_vars`) found no other bugs: macOS coverage is actually complete
    as-is (the manifest writes mac paths directly as `<home>/Library/...`,
    with no separate mac-only placeholders to be missing), and the Proton
    prefix mapping matches the standard `steamuser` convention. Can't be
    fully confirmed without real SteamOS/macOS hardware to run against,
    though.
  - **macOS path resolution: VALIDATED 2026-07-20** on real hardware (the
    dev Mac this repo started on): real DOS2 Definitive Edition saves
    resolve, capture, sync (stable in-sync across runs), and dry-run-restore
    correctly; selftest's full capture→upload→restore roundtrip runs
    natively on macOS. Remaining macOS work is packaging only (Phase 4).
  Remaining in Phase 2 (needs SteamOS/Linux hardware to validate):
  Proton path mapping, cross-OS normalization, Flatpak packaging.
- **Phase 3 — Seamless daemon (~2–2.5 wks)**: DONE 2026-07-20 (Windows +
  macOS; Linux paths written but unvalidated per 2026-07-20 direction).
  - **Launch-wrapper mode: DONE 2026-07-20.** `yasgm run [--app <id>] --
    <command…>`: pre-launch sync → run game (stdio inherited) → post-exit
    sync → child's exit code propagated. AppID comes from `--app` or Steam's
    `SteamAppId`/`SteamGameId` env vars (set for `%command%` wrappers).
    Sync failures never block the launch (worst case: post-exit conflict
    flow reconciles, non-destructively). Everything after `--` is passed to
    the game verbatim so game flags are never parsed as ours (unit-tested);
    verified live: pre/post sync around a stub game, env-based AppID
    detection, exit code 3 propagated.
  - **Watch daemon: DONE 2026-07-20.** `yasgm watch [--settle <secs>]`
    (foreground for now): initial sync pass, FS-watches all resolved save
    roots (`notify` crate), debounces changes, and syncs a game when its
    saves settle **and it isn't running** — or immediately when it exits.
    Rescans every 5 min for save dirs that didn't exist at startup.
    Verified live on macOS: change during a running game deferred, synced
    on exit; idle change synced after settle; test versions cleaned up.
  - **Running-game detection** (`running.rs`), layered per design:
    Windows `HKCU\...\RunningAppID` via winreg; Linux `~/.steam/registry.vdf`
    (+ Flatpak path) — *written but unvalidated, per decision to defer Linux
    testing*; process-scan fallback on all OSes (sysinfo: any process whose
    exe is under a game's `steamapps/common/<installdir>`).
    **Finding (2026-07-20): macOS registry.vdf has NO RunningAppID key** —
    process scan is the *primary* macOS signal, not a fallback. (Test-lab
    note: macOS SIP kills signed system binaries copied elsewhere — fake
    game processes for testing must be compiled, not copied from /bin.)
  - Per user direction 2026-07-20: **proceed on Windows+macOS only**; all
    Linux code paths are written and kept current but unvalidated until
    hardware is available.
  - **Tray status + opt-in autostart: DONE 2026-07-20.** `watch --tray`
    (`src/tray.rs`, Windows + macOS via `tao`/`tray-icon`; Linux tray
    deferred — needs GTK stack) runs the watch loop on a worker thread and
    the OS event loop on the main thread, with a generated dot icon (no
    bundled assets) and a menu (status line + Sync now / Pause / Quit)
    wired to the loop via new `WatchCommand`/status `mpsc` channels.
    `yasgm autostart [on|off|status]` (`src/autostart.rs`, D11): macOS
    LaunchAgent plist (`~/Library/LaunchAgents/dev.yasgm.watch.plist`,
    loaded immediately via `launchctl`), Windows HKCU `Run` key, Linux XDG
    autostart `.desktop` entry (written, unvalidated) — all point at
    `watch --tray` (headless `watch` on Linux). This also covers
    daemonization: the login item *is* the background process, no separate
    service framework needed. Verified live on macOS: `autostart status`
    correctly reports off; `watch --tray` run for several seconds showed
    the initial sync pass in the log and the generated icon actually
    rendered in the real menu bar (confirmed via screenshot) next to the
    other status icons.
- **Phase 4 — Polish & reach (~2–3 wks)**: IN PROGRESS.
  - **LocalFolder provider: DONE 2026-07-20.** Retrofitted D8's "modular
    provider trait" (previously aspirational — `Store` called `onedrive::`
    functions directly) into a real `Provider` trait (`src/provider.rs`:
    `exists`/`download`/`upload`/`delete`); `Store` now holds
    `Box<dyn Provider>`. `OneDriveProvider` (`src/onedrive.rs`) adapts the
    existing Graph functions with no behavior change. New
    `LocalFolderProvider` (`src/local.rs`) points at any directory —
    formalizes the "Local data location" caveats already in this doc
    (Syncthing- or desktop-client-synced folders as a sync target) —
    writing via tmp-file-then-rename so nothing observes a partial write.
    Global (not per-game) selection via `yasgm provider [onedrive|local
    <path>|status]`, stored in `config.json` (`ProviderConfig`, defaults to
    `Onedrive` so existing configs need no migration). `auth`/`status`
    branch on the selected provider: local needs no sign-in, `status` does
    a real directory + writability probe instead of a Graph call.
    Unit-tested (`src/local.rs`): upload/download roundtrip through nested
    paths, no leftover `.tmp` file, overwrite, idempotent delete. Verified
    live: pointed the provider at a scratch folder, ran a real
    `backup 435150` against actual DOS2 saves — cloud layout on disk
    matched the documented `accounts/<id>/games/<appid>/{index.json,
    versions/*.zip}` spec exactly — then `versions`, `sync` (reported in
    sync), `pin`/`unpin`/`rm` (confirmed the zip was actually removed from
    disk) all worked through the same CLI commands as OneDrive; reverted
    the live config back to `onedrive` afterward.
  - **GUI framework: Tauri v2 shell scaffolded 2026-07-20**, resolving the
    open "UI plan" question in favor of Tauri v2. Lives in `ui/` (separate
    `npm`/`cargo` project, scaffolded via `create-tauri-app`; `ui/src`
    vanilla HTML/JS frontend, `ui/src-tauri` Rust backend, package/crate
    both named `ui`) — deliberately outside the root Cargo package so it
    doesn't affect the CLI's build. A minimal version browser: table of
    cloud versions (game, id, machine, OS, files, size, active/pinned
    marker) with a per-row Restore button. The Rust backend doesn't link
    the core crate as a library yet (that's not split out from the `yasgm`
    bin today) — it shells out to the built `yasgm` binary instead:
    `list_versions` runs `yasgm versions [appid] --json` (new `--json`
    flag added to `versions_cmd` in main.rs, emitting one JSON array;
    status/progress text already went to stderr via `eprintln!`, so stdout
    was already clean for parsing) and `restore_version` runs
    `yasgm restore <appid> --version <id>`, surfacing its exit code/stderr
    as the JS-side error. If this framework choice sticks, the natural next
    step is splitting the core crate into lib+bin so the GUI can link it
    directly instead of shelling out. Verified live: `npm run tauri dev`
    opened a real window titled "YASGM — Version Browser" that loaded the
    actual two cloud versions of DOS2 from the real OneDrive store through
    the IPC → subprocess → `Store` path, correctly marking the active one
    and disabling its Restore button (confirmed via screenshot). Did not
    click-test Restore itself against the real save file — that code path
    was already proven in Phase 1's `selftest` sync matrix, and clicking it
    here would have durably swapped the real DOS2 cloud head to an older
    save; not worth the risk to real data for what would be a redundant
    check.
  - **GUI: per-game settings + pin/unpin/rm: DONE 2026-07-20.** Added a
    "Games" section above the version browser: table of installed games
    (name, AppID, mode dropdown, keep count with "default (N)" placeholder
    when unset) with Save/Reset per row, backed by a new `--json` flag on
    `config_cmd`'s list branch (same stderr/stdout split as `versions
    --json`) plus `set_game_config`/`clear_game_config` Tauri commands
    wrapping `yasgm config <appid> --mode --keep` / `--clear`. The versions
    table gained Pin/Unpin and Delete buttons (`set_pinned`/
    `remove_version`, wrapping `yasgm pin`/`unpin`/`rm`) alongside Restore;
    Delete is disabled for the active version to avoid a stray click
    deleting the version you're currently restoring from (D5/D14 don't
    require this — it's just a GUI guard rail). Note: the Keep field has no
    "clear override, keep mode override" affordance — leaving it blank on
    Save just omits `--keep` from the CLI call (existing keep untouched),
    matching the CLI's own behavior; only Reset clears both together via
    `--clear`. Verified live: `npm run tauri dev` showed both real installed
    games with correct auto-mode/default-keep placeholders, and the
    versions table's active-row Restore/Delete disabled state, all from
    real data (screenshot-confirmed). Didn't click-test Save/Pin/Delete
    live against real state — each wraps a CLI path already verified
    directly earlier (`config`, `pin`/`unpin`/`rm` against the LocalFolder
    provider test), and the IPC argument-naming convention (`appId` →
    `app_id`) was already proven live by `list_versions`/`restore_version`.
  - **`.ludusavi.yaml` local manifest overrides (D7): DONE 2026-07-21.** See
    [Local manifest overrides](#local-manifest-overrides) below for the
    what/why/schema; implementation notes here. `manifest::load()` now also
    reads an optional local file (`local_manifest_path()`: platform config
    dir, `yasgm/ludusavi.yaml`; absent by default, not an error) in the
    exact same `Manifest` schema as the community manifest and merges it in
    via `merge_manifest`: a name matching an existing entry deep-merges
    (`files` unioned per template, override's rule wins on a template
    collision; `steam`/`cloud` replaced only when the override sets them —
    a known limitation is there's no way to *remove* a bad primary file
    rule this way, only add/replace by template key, which covers the
    cases seen so far); a new name is just inserted and joins that AppID's
    merge pool normally through the existing `steam_index`/`Ctx::merged_game`
    path — no separate merge logic needed for the "two names, same AppID"
    case, since that already existed for the DOS2 base/Definitive-Edition
    situation. `doctor` prints a hint pointing at the override file path
    when any installed game isn't in the manifest. Unit-tested
    (`src/manifest.rs`): new name inserted, matching name unions files
    without duplicating the entry, override steam ID replaces missing
    metadata. Verified live: wrote a real override file adding a custom
    save path to the installed Terraforming Mars entry and a wholly new
    unlisted game with a Steam ID — `doctor` showed the new path under
    Terraforming Mars, manifest total went from 52829→52830 games (one
    new entry, not two, confirming the existing entry was extended rather
    than duplicated) and 48598→48599 with Steam IDs (confirming the new
    entry was indexed); reverted the file afterward.
  Remaining in Phase 4: decide whether/when to split the core crate into
  lib+bin for direct GUI linkage (currently shells out to the `yasgm`
  binary), macOS packaging (unsigned initially, D16), self-update,
  additional cloud providers beyond OneDrive/LocalFolder.

## Risks

- Cross-OS path normalization edge cases (Wine profile quirks, case
  sensitivity, `<storeUserId>` differing per machine). Mitigation: snapshot
  everything; a bad mapping is a misplaced restore, never data loss.
- Manifest inaccuracies for niche titles. Mitigation: non-destructive
  semantics, per-game disable, `doctor` command surfacing resolved paths.
- Detection gaps for oddball games. Mitigation: FS-watch backbone + opt-in
  wrapper.
- Graph API throttling. Mitigation: one zip per snapshot, ETag caching,
  backoff.
- PCGW licensing if ever commercialized (currently fine: MIT tool, runtime
  data fetch, attribution).

## Appendix: Azure app registration for OneDrive (one-time setup)

Steps match the modern Entra portal (verified 2026-07). Free; no Azure
subscription; **not** the Microsoft 365 Developer Program (skip that signup if
offered).

1. https://aka.ms/appregistrations (or portal.azure.com → "App registrations"),
   sign in with the personal Microsoft account that will own the app. A free
   "Default Directory" tenant is auto-created on first use.
2. **+ New registration**: name `YASGM`; supported account types =
   **"Any Entra ID Tenant + Personal Microsoft accounts"**; redirect URI:
   platform **"Public client/native (mobile & desktop)"**, value
   `http://localhost` (loopback PKCE). Register.
3. **Authentication → Advanced settings → "Allow public client flows" = Yes**
   → Save (enables device-code flow for Steam Deck Gaming Mode).
4. **API permissions → Add a permission → Microsoft Graph → Delegated**:
   `Files.ReadWrite.AppFolder` + `offline_access`. "Not granted" status is
   expected — users consent individually at first sign-in; no admin consent.
5. Copy **Application (client) ID** from Overview → goes into the code as the
   public client ID (not a secret). Never create a client secret for this app.

Auth endpoints at runtime: **`https://login.microsoftonline.com/consumers`**
(NOT `/common` — requesting the AppFolder scope through `/common` fails with
`server_error` for personal accounts; verified 2026-07-19). Scopes:
`Files.ReadWrite.AppFolder offline_access`.

Registered client ID (public, ships in binary):
`a79772b2-0da9-4af5-bc70-0aed51abab0b`.

Scope gotcha (verified): `Files.ReadWrite.AppFolder` cannot read `/me/drive`
(drive metadata/quota is out of bounds) — all Graph calls must stay under
`/me/drive/special/approot`. Quota warnings must rely on upload errors
(507/insufficient storage) rather than proactive quota reads.

## Sources

- https://github.com/mtkennerly/ludusavi
- https://github.com/mtkennerly/ludusavi-manifest
- https://github.com/mtkennerly/ludusavi/blob/master/docs/help/cloud-backup.md
- https://github.com/DavidDeSimone/OpenCloudSaves
- https://github.com/GedasFX/decky-cloud-save
- https://www.pcgamingwiki.com/wiki/PCGamingWiki:API
- https://community.pcgamingwiki.com/topic/5071-retrieve-config-data-and-save-data-location-through-the-api/
- https://learn.microsoft.com/en-us/graph/onedrive-sharepoint-appfolder
- https://learn.microsoft.com/en-us/onedrive/developer/rest-api/concepts/special-folders-appfolder?view=odsp-graph-online
- https://steamcommunity.com/discussions/forum/1/1621726179573666840/
