# YASGM (Yet Another Save Game Manager) — Design Document

> **Living document.** Updated whenever a decision is made or the plan changes.
> Last updated: 2026-10-19

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
| D16 | Apple Developer signing | ~~Deferred — ship unsigned~~ → **Superseded 2026-10-19: macOS builds are signed with a Developer ID Application certificate and notarized.** See [macOS signing & notarization](#macos-signing--notarization). |
| D17 | Name | **"Yet another save game manager" (YASGM)** (decided 2026-07-17, replacing earlier *Bonfire* pick). Binary `yasgm`, OneDrive folder `Apps/YASGM/`, wrapper `yasgm run -- %command%`. Availability checked 2026-07-17: no crates.io crate, no GitHub project with the name — fully free. |
| D18 | Offline mode | If the primary provider is unreachable, snapshots fall back to a **local pending store** (fixed path, not the D13 user-configurable one) instead of failing outright. Pending versions are **visible and restorable** (not just a silent queue) — merged into the normal version list with a `pending` flag — and auto-flush into the primary provider at the start of the next sync/backup run. |
| D19 | Steam Deck Gaming Mode UI | **Decky Loader plugin is a v1.0 release blocker** (decided 2026-10-19; previously "v2, later"). Gaming Mode is where Deck users live — a desktop-mode-only GUI doesn't count as SteamOS support. |
| D20 | Auto-update | **Required for v1.0** (2026-10-19). Desktop (Windows/macOS): **`tauri-plugin-updater`** — supersedes the earlier `self_update`-crate plan, since the shipped artifact is now the Tauri app with the CLI as a bundled sidecar (one update replaces both). SteamOS: **in-plugin self-update** from GitHub Releases, since the plugin is sideloaded (D21). See [Auto-update](#auto-update). |
| D21 | SteamOS distribution | **Decided 2026-10-19: sideloaded, personal use — not submitted to the Decky store.** The Decky plugin zip is published on GitHub Releases and installed via Decky's developer-mode "Install from URL"/zip. It *bundles* a statically linked musl `yasgm` binary in `bin/` and runs the watch daemon as a child of the plugin backend. Updates: **in-plugin self-update** from GitHub Releases (Decky's store-based updater doesn't apply off-store). No Flatpak for v1.0. |

## Open questions

- **Updater signing key (D20)** — owner generates it (2026-10-19, see
  [Updater signing key](#updater-signing-key)); public key then goes into
  `tauri.conf.json`.
- **macOS signing credentials (D16)** — owner obtains Developer ID
  certificate + notarization API key.

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
- **Existing game name** → its `files` **replace** the community entry's
  save rules (changed 2026-10-20 from a union, so a wrong upstream path can
  be removed, not just supplemented); `steam`/`cloud` replaced only if the
  override sets them. Other community entries sharing the same AppID are
  untouched.
- **Entry with empty `files`** (`files: {}`) → ignored entirely. That's the
  placeholder state of the generated skeleton:
  `yasgm manifest-gaps --skeleton` prints one entry per game whose save
  locations can't be mapped across OSes, with the community locations and
  the unmappable OS pairs as comments, plus a header explaining the
  format, the placeholders and a filled example. Written to the dev Mac's
  `~/Library/Application Support/yasgm/ludusavi.yaml` (38 games) on
  2026-10-20 for manual fixing; verified that a filled entry (Barony)
  replaced the community rules and left the gap list, and that unfilled
  entries change nothing.

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

Researched 2026-10-19 (D20, D21).

### Desktop (Windows, macOS): `tauri-plugin-updater`

- The release artifact is the Tauri app; the `yasgm` CLI ships inside it as
  a sidecar, so one app update replaces both. This is why the earlier
  `self_update`-crate plan is dropped — it would update a standalone CLI
  binary that users no longer install separately.
- **Feed**: a static `latest.json` attached to the newest GitHub Release
  (`viktor-malieichyk/yasgm`), endpoint
  `https://github.com/viktor-malieichyk/yasgm/releases/latest/download/latest.json`.
  `tauri-apps/tauri-action` in GitHub Actions builds per-platform bundles,
  signs the update artifacts, and generates `latest.json`. No server needed.
- **Signatures are mandatory** (minisign/ed25519, public key in
  `tauri.conf.json` → `plugins.updater.pubkey`). This is independent of OS
  code signing: required alongside macOS Developer ID signing (D16) and
  equally for unsigned Windows builds. Key custody: see Open questions.
- **Artifacts**: macOS `.app.tar.gz` (+ `.sig`); Windows NSIS installer
  (+ `.sig`), installed per-user so updates need no UAC prompt.
- **UX**: check on start + every 24 h (opt-out in Settings), show "Update
  available — vX.Y.Z" with release notes and an **Install & restart**
  button; never install silently mid-sync. Must **stop and restart the
  `watch --tray` daemon** around the swap (its autostart entry points at
  the sidecar inside the app bundle, so the daemon binary is replaced too).
- macOS builds are signed + notarized (D16 superseded), so in-place `.app`
  replacement by the updater is the standard, supported Tauri path. Still
  test an 0.x.y → 0.x.z update end-to-end before relying on it.
- winget/scoop/Homebrew cask: optional later, they'd consume the same
  release artifacts.

### macOS signing & notarization

Requirements (D16 superseded 2026-10-19):

1. **Apple Developer Program** membership (paid, individual is fine).
2. **Developer ID Application** certificate (not "Apple Development" or
   "Mac App Distribution" — those don't work outside the App Store). Create
   in Xcode → Settings → Accounts → Manage Certificates → + → Developer ID
   Application (or via developer.apple.com with a CSR from Keychain
   Access). Lands in the login keychain; identity string looks like
   `Developer ID Application: Viktor Malieichyk (TEAMID)`. Export as
   `.p12` only if CI will sign.
3. **Notarization credentials** — preferred: an App Store Connect API key
   (Users and Access → Integrations → Team Keys, role Developer): Issuer
   ID, Key ID, and the `.p8` file (downloadable once). Alternative: Apple
   ID + app-specific password + Team ID.
4. **Tauri wiring**: `bundle.macOS.signingIdentity` set to the identity
   (or `APPLE_SIGNING_IDENTITY` env); notarization env
   `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_PATH`. Tauri enables
   the hardened runtime, signs the bundled `yasgm` sidecar too, notarizes,
   and staples. CI additionally needs `APPLE_CERTIFICATE` (base64 `.p12`)
   and `APPLE_CERTIFICATE_PASSWORD`.
5. Side benefit: the autostart LaunchAgent (D11) shows the developer name in
   macOS's "Background Items Added" notification instead of an unknown
   binary.

Windows stays unsigned (personal use; SmartScreen warning on first install
only — the updater's own signature check is what protects updates).

### Updater signing key

Generated once by the owner (D20):

```sh
cd ui
npx tauri signer generate -w ~/.tauri/yasgm-updater.key
```

Prompts for a password; writes `~/.tauri/yasgm-updater.key` (private) and
`~/.tauri/yasgm-updater.key.pub` (public). The public key's contents go
into `tauri.conf.json` → `plugins.updater.pubkey` (committed). The private
key + password go into a password manager (and, if CI builds releases,
GitHub secrets `TAURI_SIGNING_PRIVATE_KEY` / `_PASSWORD`). Never commit
the private key. Losing it = no further auto-updates for installed copies.

### SteamOS: the Decky plugin store (how Decky updates work)

From Decky Loader's source (`frontend/src/store.tsx`, `plugin-loader.tsx`,
`backend/decky_loader/updater.py`, `browser.py`) and the plugin database
repo, verified 2026-10-19:

- **Publishing**: the store is `SteamDeckHomebrew/decky-plugin-database`,
  where each plugin is a **git submodule pinned to a commit**. Releasing an
  update = PR bumping the submodule (and `package.json` `version`). Store
  CI builds the plugin zip from source (frontend via pnpm/rollup; custom
  backends via `backend/Dockerfile` + `entrypoint.sh` on SteamDeckHomebrew's
  `holo-toolchain-rust` image, outputs from `backend/out/` land in the
  zip's `bin/`). Updates land on `testing.deckbrew.xyz` first, need a
  third-party test report (custom-backend plugins must be tested on SteamOS
  **Preview** unless all deps are statically linked — another reason for a
  static musl build), then core-team approval promotes to the main store.
  Turnaround is human-paced (days to weeks per release).
- **Client side**: Decky fetches `GET <store>/versions` (name →
  `[version, sha256, artifact]`) on load and every 6 h, compares against
  each installed plugin's `package.json` version, and shows a toast + badge;
  the user installs updates from Decky settings ("Update all" available).
  Not silent; users can "freeze" a plugin to stop update prompts. The zip's
  sha256 is verified against the store's hash before extraction.
- **`remote_binary`** in `package.json` (`name`, `url`, `sha256hash`) lets a
  plugin pull a prebuilt binary at install time; reviewers vet the URL's
  legitimacy. We'd prefer building from source in the store's CI instead.
- **Prior art**: `decky-ludusavi` (same author as `decky-cloud-save`) is a
  thin front-end that runs `flatpak install --or-update` for the Ludusavi
  Flatpak and shells out to it — two update channels (store + Flatpak).
  `decky-cloud-save` downloads a pinned rclone in its backend Dockerfile.
  We pick the bundle-the-binary approach (D21) to keep one channel.
- **Constraint found**: the store's plugin-addition PR template requires
  attesting that generative AI did not write most of the code. Moot: per
  D21 the plugin is sideloaded for personal use and never submitted.

### SteamOS: in-plugin self-update (sideloaded, D21)

Since Decky only update-checks plugins against its store, the plugin
updates itself:

- Release CI attaches `yasgm-decky.zip` + `yasgm-decky.zip.sha256` (and a
  minisign `.sig`, reusing the D20 updater key) to each GitHub Release.
- The plugin backend checks
  `api.github.com/repos/viktor-malieichyk/yasgm/releases/latest` on load
  and every 6 h (matching Decky's own cadence); compares to its
  `package.json` version; the QAM panel shows "Update to vX.Y.Z".
- On confirm: stop the daemon, download zip, verify sha256 **and**
  signature, extract to a temp dir next to
  `~/homebrew/plugins/YASGM`, atomically swap directories, then reload the
  plugin (restart `plugin_loader` service — needs root; Decky plugin
  backends run with the `root` flag or call Decky's own reload — pick
  whichever works on real Decky, verify on the VM). Keep the previous dir
  as `YASGM.bak` for rollback.
- Never self-update mid-sync; never auto-apply without a click.

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
4. **Decky plugin — required for v1.0 (D19).** See [Decky plugin](#decky-plugin-steamos-gaming-mode).

### Decky plugin (SteamOS Gaming Mode)

- **Layout**: standard `decky-plugin-template` (React + `@decky/ui`
  frontend, Python `main.py` backend provided by Decky), plus
  `backend/` building `yasgm` for `x86_64-unknown-linux-musl` (static, no
  glibc coupling to SteamOS versions) into `bin/yasgm`.
- **Backend (`main.py`)**: starts `bin/yasgm watch` as a long-running child
  in `_main`, stops it in `_unload`; all queries/actions shell out to
  `bin/yasgm … --json` — **the same subprocess + JSON seam the Tauri GUI
  already uses**, so no new API surface. Runs as the `deck` user (no `root`
  flag); confirm `HOME`/XDG env inside Decky's sandbox resolves to
  `/home/deck` (Decky exposes `DECKY_USER_HOME`).
- **Quick Access panel UI**: status line (last sync, errors, pending/
  offline count), Sync now, Pause; game list with mode (sync/backup/off);
  per-game versions with Restore/Pin; Settings: provider, keep count.
- **Sign-in in Gaming Mode**: OneDrive device-code flow rendered as a
  **QR code** of `https://microsoft.com/devicelogin` + the user code, so
  the user signs in on a phone. `yasgm auth --device` needs a `--json`
  progress mode (code, URL, expiry) for the plugin to render it.
- **Better game start/stop detection on Deck**: the frontend can subscribe
  to Steam's own app-lifetime notifications
  (`SteamClient.GameSessions.RegisterForAppLifetimeNotifications`) and
  forward start/exit to the daemon — authoritative, replacing the
  unreliable `registry.vdf` `RunningAppID` polling on this platform. Needs
  a small control channel into `watch` (e.g. a local socket or a command
  file) — the first real reason to add IPC to the daemon.
- **Toasts**: "Saves synced", "Conflict — both versions kept" (D5),
  "Cloud newer — restored before launch".
- **Single-daemon guard (new requirement, all platforms)**: a lockfile in
  the data dir so the Decky-spawned daemon, an autostart `watch --tray`,
  and a future Flatpak can never run two watchers over the same saves.

## Rust stack

`tokio`, `reqwest` (Graph), `oauth2` (PKCE + device code), `serde_yaml`
(manifest), `keyvalues-parser` + binary-VDF parsing (`appinfo.vdf`), `notify`,
`sysinfo`, `zip`, `tray-icon`. GUI: Tauri v2 + `tauri-plugin-updater`
(D20). Decky plugin: TypeScript/React (`@decky/ui`, `@decky/api`) +
Python `main.py`.

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
  - **Proton path mapping: unit-tested 2026-07-26**, upgrading the above
    code review to something executable — none of `resolve.rs`'s mapping
    logic is gated by `cfg(target_os = ...)`, it's pure string substitution
    parameterized by the `Os` enum, so it's testable on any machine against
    a hand-built fake Steam library (`steamapps/compatdata/<appid>/pfx/
    drive_c/users/steamuser/...`), no real Proton prefix or Linux box
    needed. Four tests added to `src/resolve.rs`: a Windows-only rule maps
    into the compatdata prefix correctly (verified both the resulting
    string *and* that `existing_matches` finds a real directory created at
    that exact path); an embedded wildcard (`Slot<storeUserId>Data`,
    mirroring DOS2's real manifest shape, not a whole-segment wildcard)
    still resolves and globs to a real directory through the Proton
    prefix; macOS never claims `Via::Proton` for a Windows-only rule
    (reports out-of-scope instead, matching the already-live macOS
    behavior); and an unconstrained/native rule on Linux is never routed
    through the Proton prefix at all. What this still can't cover: real
    Wine/Proton actually writing files there, or SteamOS-specific quirks —
    only that YASGM's own mapping arithmetic is correct, which was the
    part previously resting on code review alone.
  - **First run on SteamOS (steamac VM, 2026-10-20).** SteamOS 0.4.5
    aarch64 (`BUILD_ID=20261007.6125817`), user `steamos`. Binary: static
    `aarch64-unknown-linux-musl` cross-built on the Mac with
    `cargo zigbuild` (zig from Homebrew; plain `cargo build --target`
    can't link ring's C code). Gotcha: Homebrew's `rustc` shadows rustup's
    and has no musl std, so put `$(dirname $(rustup which rustc))` first in
    PATH. Copied over `scp -P 2222` to `~/yasgm-dev`. Results: Steam root
    `~/.local/share/Steam` found; Proton save path resolved inside the
    prefix for Stronghold: Definitive Edition
    (`compatdata/2140020/pfx/drive_c/users/steamuser/AppData/LocalLow/…`,
    57 files, 42.5 MB) with Steam Cloud detected → backup-only; install-dir
    rules for two other Proton games resolved. Sync with a LocalFolder
    provider uploaded 59 files (30.6 MB); after deleting a save, restore
    brought back all 59 files and the tree's sha256 matched the pre-sync
    one exactly; a pinned safety version of the pre-restore state was kept.
    Icons came from Steam's librarycache on SteamOS too. Fix found:
    Proton 11.0 and Steam Linux Runtime were listed as games ("not in
    manifest") — `installed_games` now skips any app whose install dir has
    a `toolmanifest.vdf` (every Steam compatibility tool ships one, games
    don't; unit-tested). VM access setup: SSH must be enabled in the
    launcher (Settings → Advanced) — it boots with `steamac.ssh=0` otherwise
    and runtime-masks sshd, so `systemctl enable` fails; `passwd` for the
    `steamos` user, then `ssh-copy-id -p 2222`.
  - **Cross-OS restore Mac ↔ SteamOS (2026-10-20)** through the real
    OneDrive (VM signed in via `auth --device`, same Steam account
    57948699), using Baba Is You (native on both: Mac
    `<home>/Library/Application Support/Baba_Is_You`, Linux
    `<xdgData>/Baba_Is_You`). **Bug found**: restore re-expanded the
    snapshot's recorded template ignoring its OS constraint, so a Mac
    version restored on Linux was written to
    `/home/steamos/Library/Application Support/Baba_Is_You` ("restored 4
    files", game never sees them). Fix: `resolve::restore_target` — a
    mount's own rule is used only if it applies natively on this OS (or is
    OS-neutral, e.g. `<base>/save`); otherwise it's swapped for this OS's
    rule with the same *location key* (template minus its OS-specific base
    dir: `<home>/Library/Application Support/`, `<xdgData>/`,
    `<winAppData>/`, …, compared case-insensitively). Ties: a candidate
    whose data — or for Proton, whose `compatdata/<appid>/pfx` — already
    exists wins, then native over Proton (a Windows version restored where
    the game runs natively on Linux goes to the native dir, not an absent
    prefix). No counterpart → the mount is skipped with a message, never
    guessed. `extract` now takes the game's manifest rules; restore prints
    `p0: <old> → <new> on this OS` when it remaps. Unit-tested (mac→linux,
    windows→mac, windows→linux native vs. Proton-prefix, no-counterpart
    skip, neutral rule). Verified live both ways with VM saves moved
    aside: Mac version → VM wrote 4 files to `~/.local/share/Baba_Is_You`,
    sha256 identical to the Mac's; a VM version (plus a marker file) → Mac
    wrote 5 files to `~/Library/Application Support/Baba_Is_You`, all
    matching, with the Mac's prior state kept as a pinned version. Test
    versions and marker removed afterwards; both sides "in sync".
  - **Tiered cross-OS matching (2026-10-20).** The base-dir key alone
    bridged only 30% of OS pairs in the manifest (3,135 Steam games with
    saves on 2+ OSes), so `restore_target` now compares *locations*
    (`location_parts`: base dir stripped — incl. `<home>/AppData/*`,
    `unity3d/`, `My Games/`, `Saved Games/` —, a trailing file pattern
    dropped, a macOS `X.app/Contents/Resources` dropped, leading dot
    dropped, `unity.`/`com.Company.Product` split) in tiers: (1) same
    location; (2) nested — one is a subfolder of the other (Terraria's
    `Terraria` vs `Terraria/Players/*.plr`, RimWorld), restore adds or
    drops the extra level; (3) same last two folder names or same
    distinctive last name (≥4 chars, not generic like `saves`/`data`;
    Hades' `Supergiant Games/Hades` vs `Saved Games/Hades`); (4) exactly
    one save *location* on each side. Only OS-specific rules that apply to
    Steam are candidates (Microsoft Store `Packages/…/wgs` rules and
    OS-neutral rules excluded; a rule tagged for several OSes counts for
    each). `extract` now gets the version's OS so tier 4 counts the right
    side. Remapped single files keep their own name inside the chosen
    directory; a directory remapped onto a file pattern restores into the
    pattern's directory. Result over the whole manifest: **38 of 3,135
    games** still have an OS pair with no match (was 287 after the first
    pass); `yasgm manifest-gaps [--json|--explain]` (dev) lists them /
    every chosen mapping with its tier (6,709 tier 1, 311 tier 2, 2,215
    tier 3, 1,500 tier 4). Sampled tier 3/4 mappings looked right; the
    weakest kind is a macOS Unity `unity.….plist` prefs file paired with a
    Linux folder (lands there, but prefs formats differ). Warnings for the
    remaining games: `doctor` prints `⚠ cross-OS: …`, `config --json`
    carries `cross_os_note`, the GUI shows it under the game header, and
    `restore` warns before restoring a version from an OS it can't map.
    Unit tests use the real rule shapes of Hades, Celeste, Slay the Spire,
    Terraria, RimWorld, Nine Sols, Warhammer II, 911 Operator.
  - **Headless `watch` daemon on SteamOS (steamac VM, 2026-10-20).**
    Lock verified (second instance → exit 3 with the right pid; `setsid`-
    detached daemon survives the SSH session). Three bugs found and fixed:
    (1) **watched too much**: each save root's *parent* was watched
    recursively and any change under it counted — for Baba Is You all of
    `~/.local/share` (unrelated `sddm` writes triggered syncs), descending
    into Proton prefixes whose `dosdevices/z:` links to `/` ("permission
    denied … z:/efi"). Now `WatchRoot`: a save dir is watched recursively,
    its parent only non-recursively (to see the dir itself vanish/appear);
    a save file only via its folder, non-recursively; events count only
    inside the save dir or matching the rule's file pattern (so a new
    `Game3.dat` for `Game*.dat` still counts). Verified: an unrelated file
    in `~/.local/share` → nothing; a file added to / removed from Baba's
    save dir → backup after the 5 s settle each time. (2) **running games
    not detected**: SteamOS never sets `RunningAppID` in `registry.vdf`
    while a game runs, and the exe-path scan can't see Proton (Wine
    loader) or FEX-emulated x86 games on ARM. `running.rs` now also reads
    Steam's launcher: every Linux game runs under `reaper SteamLaunch
    AppId=<id> -- …` for the whole session, and game processes inherit
    `SteamAppId=<id>` (cmd/environ via sysinfo, Linux only) — the same
    signal Steamback's desktop mode uses (Decky plugins use
    `SteamClient.GameSessions.RegisterForAppLifetimeNotifications`
    instead; to adopt in the Decky plugin). (3) **synced mid-launch**:
    Proton runs a short setup step under the launcher, exits, and starts
    the real game ~3 s later, so the daemon saw an exit in between.
    `GameSession` now only reports an exit after the game has been gone
    10 s (`EXIT_GRACE`), and keeps it "running" meanwhile so settled saves
    wait too; switching to another game exits the previous one at once.
    Verified with Stronghold (Proton): the Steam Cloud write at launch was
    held, Steam ended the game at 06:09:53 and the daemon logged one exit
    and synced at 06:10:05. Unit tests replay the observed launch.
  Remaining in Phase 2 (needs SteamOS/Linux hardware to validate):
  cross-restore Windows ↔ SteamOS via a Proton prefix (needs a Windows
  machine); sleep/resume; real Deck (x86_64, user `deck`). (Flatpak packaging moved to post-1.0 — SteamOS ships via
  the Decky plugin, D21.)
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
  - **Lib+bin split: deferred 2026-07-21.** Decided to keep the GUI
    subprocess-based (shells out to the `yasgm` binary, parses `--json`
    output/exit codes) rather than splitting the core crate into lib+bin
    for direct linkage. Reasoning: nothing the GUI does today (versions,
    restore, config, pin/rm — all fire-and-forget actions) is hurt by
    process-spawn latency or string-parsed errors, while the split is real
    API-design work (today's CLI functions interleave arg parsing,
    `println!` formatting, and actual logic, so splitting means untangling
    that, not just adding `pub`) that would also remove the current
    isolation between the CLI and GUI binaries. Revisit when either
    trigger condition shows up: (a) the GUI wants live progress during a
    sync, which text-scraping a subprocess can't give cleanly, or (b)
    macOS packaging forces resolving the binary-location question anyway
    (`ui/src-tauri/src/lib.rs`'s `yasgm_path()` currently hard-codes a dev
    build path with a PATH fallback, which won't survive being bundled
    into a `.app`) — at that point the seam is being touched regardless,
    so it's the natural point to reconsider.
  - **macOS packaging (D16): DONE 2026-07-21** — unsigned, as decided.
    `ui/scripts/build-sidecar.sh` builds the `yasgm` CLI in release mode and
    copies it into `ui/src-tauri/binaries/yasgm-<host-triple>` under
    Tauri's sidecar naming convention; wired as both `beforeDevCommand` and
    `beforeBuildCommand` in `tauri.conf.json` so it's always fresh and
    matches the current host without a manual step. `bundle.externalBin`
    bundles it into the `.app`; the Rust backend
    (`ui/src-tauri/src/lib.rs`) now invokes it via `tauri-plugin-shell`'s
    `app.shell().sidecar("yasgm")` instead of the old dev-path-guessing
    `Command::new`, which **resolves trigger (b) of the deferred lib+bin
    split above without doing that split** — the binary-location problem
    was the actual blocker, not linkage, and Tauri's sidecar mechanism
    solves it directly. `bundle.macOS.signingIdentity: null` and
    `minimumSystemVersion: "13.0"` (matching D12). Verified live:
    `npm run tauri build` produced `YASGM.app` and `YASGM_0.1.0_aarch64.dmg`
    under `target/release/bundle/`; confirmed `Contents/MacOS/yasgm` exists
    inside the bundle; launched the built `.app` standalone (not via
    `cargo`/`npm run dev`) and it loaded real Games/Versions data through
    the bundled sidecar, proving the packaging — not just the dev-mode
    path — actually works. **Caution for next time**: during this build, a
    second copy of `YASGM.app` appeared at the real `/Applications`
    (matching timestamps/file sizes exactly) that neither the command
    history nor `bundle_dmg.sh` (read in full — it only symlinks
    `Applications` inside the mounted DMG, standard drag-to-install UX, not
    a real copy) explains; unified log search for the build window found
    nothing either. Removed it and confirmed builds stay under
    `target/release/bundle/` on a re-run, but the mechanism is still
    unexplained — worth watching for a repeat next time this is built.
  - **GUI: master-detail layout: DONE 2026-07-21.** Replaced the two
    always-visible tables (Games, Versions) with a sidebar of games
    (`#games-list`) and a detail panel (`#detail`) that shows the selected
    game's settings (mode/keep, Save/Reset) and its versions only —
    decluttering both the per-row column count (version rows now show a
    human-formatted date, a colored active/pinned badge, and a compact
    machine/OS/files/size subtitle instead of a raw version-id string and
    six separate table columns) and the always-all-games flat version list
    from before. First game auto-selected on load. No Rust/IPC changes —
    `list_games`/`list_versions`/etc. are unchanged, only how `ui/src/*`
    calls and renders them. Verified live: real Games/Versions data
    rendered correctly in the new layout; a real accessibility-tree click
    on a sidebar game correctly swapped the detail panel to that game's
    settings and versions; zoomed screenshot confirmed the active version's
    Restore/Delete render visibly washed-out (disabled) against the
    non-active version's crisp enabled buttons; fixed a Keep-input
    placeholder ("default (10)") getting clipped by the number spinner
    (`.keep-input` width 5em → 7.5em), confirmed via hot-reload screenshot.
    Also picked up incidentally: darker/more legible `prefers-color-scheme:
    dark` colors for inputs/select/buttons/badges as part of the CSS
    rewrite — not yet verified live against real dark mode (an earlier
    attempt to toggle system dark mode for testing was interrupted; system
    appearance was reverted to its original light setting and left alone).
  - **GUI: app icon, tray/minimize, Settings screen, accent color: DONE
    2026-07-21.**
    - **Icon**: procedurally generated (`ui/scripts/` doesn't hold it —
      one-off Python/Pillow script, not checked in) — white cloud +
      circular sync-arrows badge on a blue gradient rounded square,
      matching the daemon tray dot's color language. Regenerated the full
      icon set via `npx tauri icon`; deleted the iOS/Android/Windows-Store
      assets it also produces since they're out of scope (Windows 10+,
      macOS, SteamOS only per Vision).
    - **Minimize to tray**: closing the window now hides it
      (`WindowEvent::CloseRequested` → `window.hide()` +
      `api.prevent_close()`) instead of quitting; a new tray icon (built
      via `tauri::tray::TrayIconBuilder` in `setup()`, using
      `app.default_window_icon()`) with "Show YASGM"/"Quit YASGM" restores
      it or actually exits; macOS Dock-icon "reopen" also restores it
      (`RunEvent::Reopen`). This is a second, independent tray icon from
      the CLI daemon's own `watch --tray` — no unification attempted.
    - **Single-instance**: added while verifying the above, after a second
      `target/debug/ui` process turned up during testing (from not killing
      a prior dev session — a real scenario once "closes to tray instead
      of quitting" is the default, since a duplicate double-click is easy
      to end up with). `tauri-plugin-single-instance`, registered first
      per its docs; a second launch attempt just calls the same
      `show_main_window` instead of starting a process. Verified live:
      launching the built binary again while one instance was already
      running exited immediately and left exactly one process.
    - **Settings screen**: the theme picker moved off a plain `<select>`
      onto a proper Settings tab (top-bar `Library`/`Settings` nav) with a
      segmented Light/Dark/System control, plus two more settings the CLI
      already had but the GUI didn't expose: cloud provider (current
      provider display, "Check connection" running the real `yasgm
      status`, "Use OneDrive"/"Sign in…" — the latter blocks on the real
      interactive browser OAuth flow — and "Use this folder" for
      LocalFolder with a plain text path input, no native folder-picker
      dialog yet) and autostart (checkbox wrapping `yasgm autostart
      on/off`). New `--json` output added to `provider` and `autostart`
      CLI commands (same stdout/stderr split as the existing `--json`
      flags) for the new `get_provider`/`get_autostart` Tauri commands to
      parse. Found and fixed a real bug while verifying: `.hidden {
      display: none }` was defined before `.settings-page { display: flex
      }` in the stylesheet, so equal-specificity cascade let the later
      rule win and both views rendered stacked at once regardless of the
      `hidden` class; fixed by making `.hidden` `!important` (a utility
      class is supposed to always win). Verified live: Settings shows real
      provider ("OneDrive") and autostart ("off") state; "Check connection"
      returned the real Graph app-folder probe result; the Dark segment
      force-applied dark styling regardless of system appearance.
    - **System accent color**: replaced the hardcoded `#646cff` brand
      purple used for selection borders/backgrounds, the segmented
      control's selected state, links, and button-hover borders with the
      CSS4 system color keywords `AccentColor`/`AccentColorText` (Safari
      15.4+, well under the macOS 13 minimum), with a `#646cff` fallback
      inside `@supports not (color: AccentColor)`. Tinted backgrounds use
      `color-mix(in srgb, var(--accent) N%, transparent)` instead of a
      fixed rgba. Verified live: selection/segmented-control colors
      rendered as this Mac's actual (blue) system accent color, not the
      old purple, with no broken/black fallback rendering.
  - **GUI: "Open save location" / "Open backups location": DONE
    2026-07-21.** Two new CLI commands feeding two new buttons in the
    detail panel: `yasgm paths <appid>` (JSON array of real, *existing*
    resolved save directories — reuses the same `resolve::resolve_rule`/
    `existing_matches`/`measure` calls `doctor` already uses) and `yasgm
    backup-location <appid>` (`{"kind": "cloud"}` for OneDrive — there's no
    local path, backups only exist via Graph API, D8/D13 — or
    `{"kind": "local", "path": ...}` for the LocalFolder provider, which
    also creates the directory so opening it can't fail on absence). The
    GUI calls these then `tauri_plugin_opener`'s `open_path` to reveal the
    result in Finder/Explorer. Verified live: "Open save location" opened
    a real Finder window on the actual DOS2 `Savegames` directory; "Open
    backups location" correctly showed "backups are stored in OneDrive
    (cloud) — no local folder to open" rather than trying to open
    anything, matching the current OneDrive provider. **Dev-workflow
    gotcha found and worth remembering**: `tauri dev`'s
    `beforeDevCommand` rebuilding `ui/src-tauri/binaries/yasgm-<triple>`
    is not sufficient by itself to pick up CLI changes in an
    already-running `tauri dev` session — Tauri's dev-mode sidecar staging
    apparently copies that file into `ui/src-tauri/target/debug/yasgm`
    once, and doesn't re-copy just because the source file's content
    changed (no `cargo:rerun-if-changed` on it, seemingly). Symptom: the
    running GUI calls a *stale* sidecar and a brand-new CLI subcommand
    comes back "unknown command", even though invoking the rebuilt
    `binaries/` file directly works fine. Fix: delete
    `ui/src-tauri/target/debug/yasgm` and fully restart `tauri dev` (a
    hot-reload isn't enough) whenever a `tauri dev` session needs to pick
    up new sidecar-side (CLI) commands.
  - **GUI: real system accent color (fixed, superseding the earlier
    `AccentColor` attempt): DONE 2026-07-21.** The CSS `AccentColor`/
    `AccentColorText` system-color keywords from the earlier attempt
    turned out not to work — verified against a real system set to Purple
    (`AppleAccentColor` = 5) and the UI kept rendering macOS's default
    blue regardless. Replaced with a native query: `get_accent_color`
    (Rust) shells out to `defaults read -g AppleAccentColor` (the same
    NSUserDefaults key AppKit itself reads) and maps the index against
    Apple's known, stable 8-color accent palette (Graphite/Red/Orange/
    Yellow/Green/Blue/Purple/Pink; unset/multicolor falls back to macOS's
    own default blue `#0a84ff`); `text` is always white, matching how
    macOS itself renders selection/accent-button text regardless of hue
    rather than computing per-color contrast. `main.js`'s
    `loadAccentColor()` calls it once at startup and sets `--accent`/
    `--accent-text` as inline styles on `<html>`, overriding the static
    CSS fallback. Verified live (after tracking down a **second,
    JS-side instance of the sidecar-caching-style gotcha**: `tauri dev`'s
    frontend hot-reload did not reliably pick up the new `main.js` either
    — confirmed by adding temporary status-bar debug output that never
    appeared until a full `tauri dev` restart; treat "hot reload didn't
    show my change" as gotcha-shaped from now on, not proof the change is
    wrong): a real system set to Purple showed `accent color: #af52de` in
    that debug output, and the selected game item / segmented control /
    nav buttons all rendered visibly purple, matching the system's actual
    accent swatch in System Settings > Appearance observed side-by-side.
    **Incident during this verification pass**: at some point mid-session
    the real `~/Library/LaunchAgents/dev.yasgm.watch.plist` ended up
    installed and loaded (a real `watch --tray` daemon was actually
    running, PID confirmed via `ps`) even though enabling autostart was
    never a deliberate test step in this pass — mechanism not identified
    (the Settings screen's autostart checkbox only fires on a genuine
    `change` event, which programmatic `.checked` sets from
    `loadAutostart()` shouldn't trigger). Caught by noticing the Settings
    screen showing autostart on unexpectedly; fixed via `yasgm autostart
    off`, confirmed the plist, launchctl entry, and daemon process were
    all gone. Worth extra scrutiny if it recurs.
  - **Offline mode (D18): DONE 2026-07-21.** `Store` (`src/store.rs`) now
    owns a second provider internally — `pending`, a `LocalFolderProvider`
    rooted at a fixed path (`dirs::data_dir()/yasgm/pending`, independent
    of whatever primary provider the user configured) — instead of just
    wrapping one. `load_index` merges both providers' indices, tagging
    pending entries via a new `Version::pending` flag that's set only
    in-memory by the merge (`#[serde(skip_serializing)]` — never written to
    either provider's own index.json, since which store a version is in
    already implies it). `push_raw` tries the primary provider first and
    falls back to `pending` on *any* failure (network, expired auth,
    quota — no attempt to distinguish reasons, since from the user's
    perspective "couldn't reach the cloud right now" should always have
    the same safety net); `download_version`/`set_pinned`/`remove_version`
    resolve to whichever provider actually holds the version. A public
    `flush_pending` moves stranded pending versions into primary and is
    called explicitly at the start of `sync_game`/`backup_cmd` — not just
    relied on via `push_raw`'s own internal call — because a game whose
    save content hasn't changed since a prior offline backup never reaches
    `push_raw` at all (callers short-circuit on "unchanged"), so without
    an explicit call a pending version could sit stranded indefinitely
    even after connectivity returns (**found this gap live**: after
    forcing an offline backup then restoring primary access, a second
    `backup` run reported "unchanged since last version" and did nothing
    — fixed by adding the explicit `flush_pending` calls). `versions_cmd`
    gained a `pending` field in both its JSON and human output (`[pending
    — offline, not yet synced]`); `backup_cmd`/`run_sync`'s success
    messages now say "backed up locally (pending — offline)" instead of
    "uploaded" when the fallback path was taken (also found live — the
    first version of this printed "uploaded" even when the write actually
    went to local pending, since the CLI's own status message didn't check
    the returned `Version::pending`). GUI: `main.js` renders a gray
    "pending — offline" badge alongside active/pinned on any pending
    version row; Restore/Pin/Delete work on pending versions with no GUI
    changes needed beyond the badge, since `Store`'s public method
    signatures didn't change — the dual-provider logic is entirely
    internal to `store.rs`. Unit-tested (3 new tests in `src/store.rs`,
    using a test-only `AlwaysFailProvider` to simulate "primary
    unreachable" without a real network outage): fallback marks the
    version pending and it's downloadable; a stranded pending version gets
    flushed into primary on the next push; pin/unpin/rm resolve to
    whichever store actually holds a version even when primary is
    down. Verified live end-to-end against real disk I/O (LocalFolder
    provider with write permission revoked via `chmod 000` to force real
    failures, not just a mock): a real `backup` fell back to a local
    pending version, `versions --json` showed it with `pending: true` as
    the active head, a same-content second `backup` while still "offline"
    correctly did nothing new, restoring permissions and backing up again
    flushed it into primary (confirmed on disk and via `pending: false`),
    and the GUI badge rendered correctly against this same real scenario.
  - **GUI: dark mode verified live: DONE 2026-07-21.** Toggled real macOS
    system appearance to Dark (was Light; reverted after) and checked both
    views: Library (games sidebar, selected-item highlight, version rows,
    active/pinned badges, disabled-button dimming) and Settings (segmented
    Appearance control, provider/autostart sections) all render with
    legible text/borders/inputs. Incidentally reconfirmed the accent-color
    fix is live/dynamic rather than cached: the system's accent had
    changed from Purple to Green since it was last checked, and the
    segmented control picked that up correctly with no code changes.
  - **GUI: native folder picker, and "open location" buttons split into
    save/pending/cloud-provider: DONE 2026-07-25.**
    - **Folder picker**: added `tauri-plugin-dialog`; a new `pick_local_folder`
      Tauri command bridges its callback-based `DialogExt::pick_folder` API
      into an async command via a plain `std::sync::mpsc` channel (blocking
      the task while the modal is open is fine — that's the whole point).
      A "Browse…" button between the LocalFolder path input and "Use this
      folder" populates the field; the text input stays editable too.
      Verified live: a real NSOpenPanel opened as a sheet on the window and
      browsing showed the real filesystem (including the real `Larian
      Studios` folder under Documents).
    - **"Open backups location" repurposed** to open the local *pending*
      (D18, offline/unsynced) folder specifically, shown only when the
      selected game actually has a pending version — reusing the
      already-fetched `list_versions` data's `pending` flag rather than a
      separate check. New `yasgm pending-location <appid>` CLI command
      (always a real local path, since the pending store is a fixed
      fallback regardless of provider) and `get_pending_location` Tauri
      command. Verified live: visible for DOS2 after forcing an offline
      backup, hidden for Terraforming Mars (no pending versions).
    - **New "Open in cloud provider" button** — what "Open backups
      location" used to do before the repurposing above, restored as its
      own clearly-named button: opens the *primary* provider's actual
      backup location. For LocalFolder, the local folder (unchanged
      `backup_location_cmd` local branch). For OneDrive, previously a
      dead-end `{"kind": "cloud"}` message — now fetches a real Graph
      `webUrl` for the game's folder via `onedrive::item_get` and opens it
      in the browser via a new `open_url` Tauri command
      (`OpenerExt::open_url`); falls back to the app's OneDrive root
      folder's `webUrl` if this game has no backups yet, so it's never a
      dead end. Verified live against the real account: opened Chrome to
      `onedrive.live.com/my?id=.../Apps/YASGM/accounts/.../games/435150` —
      the exact real folder, confirmed via `Google Chrome`'s active tab
      URL.
    - **Incident during this verification pass**: while testing the
      folder-picker dialog, the real provider config ended up switched to
      LocalFolder pointing at a never-intended path
      (`~/Documents/_save_sync`, which also got created), even though the
      only actions taken were opening the picker and clicking "Open" in
      it — "Use this folder" was never knowingly clicked. Mechanism not
      identified (osascript click commands returned AX-resolution errors
      immediately before and after, suggesting some clicks landed
      ambiguously across rapid retries with slightly different
      accessibility paths); fixed by reverting the provider to OneDrive
      and removing the stray folder. Given this is the third such
      unexplained real-state side effect from automated UI-testing clicks
      this project (after an `/Applications` copy during macOS packaging
      and an autostart LaunchAgent getting enabled), subsequent testing
      switched to single deliberate clicks with the AX path confirmed
      *before* clicking, rather than rapid alternate-path retries after an
      error — worth keeping as the default approach for any future
      mutating-action GUI verification.
  - **GUI: icon redesign — dark squircle main icon, clean tray silhouette:
    DONE 2026-07-26.** The main icon's background changed from a light-blue
    gradient rounded square to a Steam-style dark navy-to-black squircle
    (`#1b2838` → `#0d0d0d`-ish), keeping the cloud+gamepad+sync-arrows
    motif recolored for contrast. The tray icon went through two passes:
    the first attempt (body + two plain grip circles) read as sunglasses/
    a dumbbell at real tray sizes, not a controller — replaced with a
    front-view controller silhouette (flat-topped body, two round grips
    drooping down with a visible notch between them), which reads clearly
    as a gamepad down to 16px; the main icon's gamepad glyph was updated
    to the same silhouette for consistency between the two. Regenerated
    via the same `npx tauri icon` + iOS/Android/Windows-Store-file-cleanup
    process as the original icon work. Verified live: real menu-bar
    screenshot (zoomed to actual pixels, not just the PIL preview) shows
    the controller shape rendering as designed. Also hit one purely
    testing-side flake while verifying (window took a beat to appear on
    one launch, immediately clicking "Show" or just waiting resolved it
    every other time) — not reproduced across a clean 8-second polling
    run, treated as automation timing noise rather than a product bug.
  - **Game icons + visual refresh (2026-10-20).** New `src/icons.rs` +
    `yasgm icons [appid...] [--offline] [--json] [--embed]`. Cross-platform
    by construction: Steam's own image cache
    `<Steam root>/appcache/librarycache` has the same layout on Windows,
    macOS and Linux/SteamOS — current clients store
    `<appid>/<sha1>.jpg` (the 32×32 community icon, next to `header.jpg`,
    `logo.png`, `library_600x900.jpg`, `library_hero.jpg`), pre-2024 clients
    `<appid>_icon.jpg`; both are checked. Coverage on the dev Mac: 2,507 of
    3,090 cached apps have the icon (Steam only caches what it has shown),
    all 9 installed games. Fallback: Steam's keyless store API
    (`IStoreBrowseService/GetItems`, `include_assets` → `community_icon`
    hash, 50 apps per call) + `cdn.cloudflare.steamstatic.com/steamcommunity/public/images/apps/<appid>/<hash>.jpg`
    (byte-identical to Steam's cached file), saved once to
    `<cache dir>/yasgm/icons/<appid>.jpg`; apps with no icon get a `.miss`
    marker and aren't re-queried for 7 days; an offline lookup records
    nothing. Downloaded icons are never refreshed (Steam rarely changes
    them). `--embed` adds `data:` URLs so the webview needs no file access
    or asset-protocol scope (Steam's root on Windows is arbitrary). Also:
    `find_steam_root` on Windows now tries `HKCU\Software\Valve\Steam\SteamPath`
    first, so Steam on another drive is found. GUI: new `game_icons`
    command; icons load in two passes (local cache instantly, then
    downloads), with a letter tile (per-appid hue) as placeholder. Layout
    redone as a full-height sidebar (brand, Library/Settings nav, game list
    with icon + short mode label) and a content pane of cards. Themes:
    System / Light / Dark / **Steam** (Steam client navy + `#66c0f4` blue,
    green primary buttons, square corners; native titlebar forced dark).
    Light/Dark use the OS accent via `--system-accent` (set from
    `get_accent_color`) so Steam can keep its own blue. Window default
    1000×680, min 760×480. Visually checked in all themes via headless
    Chrome with a stubbed `__TAURI__` fed real `config`/`icons` output.
  Remaining in Phase 4: auto-update (moved into the v1.0 release plan
  below); additional cloud providers beyond OneDrive/LocalFolder (post-1.0).
- **Phase 5 — Decky plugin (~2–3 wks)**: NEW 2026-10-19 (D19). Blocked on
  Linux/SteamOS hardware for real validation, but scaffolding, the musl
  build, the `--json` additions to the CLI, and the frontend can be built
  and unit-tested on the dev machine first.

## Release plan (v1.0) — reviewed 2026-10-19

v1.0 ships when all of these are done, in this order (updated 2026-10-19:
things testable without SteamOS hardware or Apple credentials first;
**macOS signing deliberately last**, so day-to-day builds stay unsigned
and fast until the release is otherwise ready). Status is tracked inline.

1. **Single-daemon lockfile** (needed by desktop autostart, the GUI's
   tray, and Decky) — so two `watch` processes can never run over the
   same saves. **DONE 2026-10-19.** `src/lock.rs`: OS advisory lock
   (`std::fs::File::try_lock` — flock on Unix, LockFileEx on Windows; no
   new crate) on `<data dir>/yasgm/watch.lock`, taken at the top of
   `watch_cmd` for both headless and `--tray` modes and held for the
   process lifetime. A second `watch` exits with code 3 and "already
   running (pid N)". PID lives in a separate `watch.pid` sidecar because a
   Windows lock also blocks reads of the locked file; it's informational
   only — the OS drops the lock when the holder dies, so a stale pid file
   never blocks (unit-tested). New `yasgm watch status [--json]` →
   `{"running", "pid"}` for the GUI updater (step 2) and Decky (step 4).
   Verified live on macOS: second headless and second `--tray` instance
   both refused with the right pid; after `kill -9` of the daemon (which
   landed mid initial sync) status reported not running, the stale pid
   file was ignored, and `state.json` was still valid. Windows locking
   path compiles from std but is unverified on hardware. Test gotcha: the
   "lock frees after drop" unit test was flaky (~1 in 3) because other
   tests spawn processes in parallel — a child forked while the fd is open
   shares the flock until its exec closes it (O_CLOEXEC) — so the test
   retries for up to 1 s. **Not covered**: a one-shot `sync`/`restore`
   from the CLI or GUI can still run concurrently with the daemon's own
   sync; a per-operation lock is a candidate follow-up.
2. **Desktop auto-update (D20)**: `tauri-plugin-updater` + Settings toggle
   + update banner + daemon stop/restart around install. **DONE on macOS
   2026-10-20; Windows unverified.** GUI (`ui/src-tauri/src/lib.rs`): `check_update`
   / `install_update` / `app_version` commands; the found `Update` is held
   in managed state so Install installs exactly what was shown;
   `YASGM_UPDATE_ENDPOINT` overrides the feed for local testing (artifacts
   are still verified against the baked-in key). Install runs
   `yasgm watch stop`, `download_and_install`, then `app.restart()`; on
   failure it restarts the daemon if it had been running. On every GUI
   start `ensure_daemon` runs `watch start` when autostart is on — this is
   what brings the daemon back after an update (side effect: opening the
   GUI also revives a daemon the user quit from the tray, if autostart is
   on). Frontend: banner with **Install & restart**, Settings → Updates
   (current version, "Check automatically" toggle stored in localStorage,
   default on, check on start + every 24 h, "Check now"). Background checks
   are silent unless an update exists. `tauri.conf.json`:
   `createUpdaterArtifacts`, pubkey (key pair in `~/.tauri/yasgm-updater.key`,
   password-protected), GitHub `latest.json` endpoint, Windows
   `installMode: passive`. CLI: `yasgm watch stop` (SIGTERM, then SIGKILL
   after 10 s; `taskkill /F` on Windows — no graceful channel yet, safe
   because all local state writes are now atomic via new `src/fsutil.rs`:
   config, state, tokens (0600 applied to the temp file pre-rename),
   manifest + etag) and `yasgm watch start` (detached, own process group /
   `CREATE_NO_WINDOW`, `--tray` on Win/mac, waits for the lock). Both are
   idempotent with `--json`; verified live on macOS (start twice → same
   pid, stop twice → `was_running` true then false, `state.json` valid).
   Build gotcha: Xcode 27's `strip` corrupts proc-macro dylibs, so both
   crates set `[profile.release.build-override] strip = false`.
   **E2E verified on macOS**: signed release 0.1.1 `.app.tar.gz` served
   from a local `latest.json`; a `--debug` 0.1.0 bundle (release builds
   reject non-https endpoints, so the test feed needs a debug build of the
   *old* version) launched with `YASGM_UPDATE_ENDPOINT`, showed the banner,
   Install & restart downloaded, verified and swapped the bundle (now
   0.1.1), relaunched, and `ensure_daemon` restarted the daemon from the
   new sidecar (new pid); `state.json` valid. Signing for builds: `source
   ~/.tauri/yasgm-signing.env` (owner-created, 0600; exports
   `TAURI_SIGNING_PRIVATE_KEY` + password) before `npm run tauri build`.
   **Remaining**: same test on Windows (NSIS passive install, `taskkill`
   stop path).
3. **CLI groundwork for Decky**: `--json` for `status`, `auth --device`
   (code/URL/expiry events), `games`; daemon control channel for external
   start/stop events; static `x86_64-` and `aarch64-unknown-linux-musl`
   builds. **DONE 2026-10-20.**
   - `status --json` → `{provider, signed_in, ok, detail, error, daemon:
     {running, pid}}`; a failed provider check is reported in the JSON
     (`ok: false`), not as a failure. `games [--json]` = the per-game
     listing (`config --json`) plus `last_version`/`synced_at` from
     state.json. `auth --device --json` → JSON lines: `code` {user_code,
     verification_uri, expires_in}, `waiting` {elapsed} every 30 s, then
     `signed_in` {detail} or `error` {message} (exit 1); `not_needed` for
     the LocalFolder provider. `onedrive::login_device` now reports through
     a `DeviceEvent` callback (text and JSON share it).
   - Control channel (`src/control.rs`): Unix socket
     `<data dir>/yasgm/watch.sock` (0600, next to `watch.lock`; stale file
     replaced on start since we hold the lock; removed on quit). One line
     per connection, one reply line (`ok` / JSON / `error: …`). Client:
     `yasgm watch send game-started <id> | game-stopped <id> | sync [id] |
     pause | resume | quit | status` (`status` → `{paused, running_game,
     pending_sync, watching_games}`). Headless and tray modes share one
     `WatchCommand` channel. Steam start/stop hints override process
     detection in `GameSession`: a hinted game is running until its stop
     hint, which syncs immediately (no grace), and its lingering processes
     are ignored for 30 s after. `watch stop` now asks for `quit` first
     (finishes a sync in progress; 30 s timeout) and only then terminates.
     Not on Windows (no Decky there): `serve` is a no-op, `send` errors.
     Verified in the steamac VM: every request, hint → one exit sync,
     clean stop in 1.9 s with the socket removed.
   - `scripts/build-linux.sh [x86_64|aarch64]` → `dist/yasgm-linux-<arch>`
     (static, stripped; 4.9 MB / 4.3 MB) via `cargo zigbuild`, using
     rustup's toolchain even when a Homebrew rustc comes first on PATH.
     CI wiring is part of step 7.
4. **Decky plugin (D19, D21)**: scaffold from `decky-plugin-template`,
   bundled `bin/yasgm` per arch, QAM UI, QR device-code sign-in,
   lifetime-notification forwarding, toasts. **CODE LANDED 2026-10-20;
   backend verified on SteamOS, real Gaming Mode test pending** (Decky
   Loader not yet installed in the VM — needs the owner's sudo).
   - `decky/` (from the official template as of 2026-04: `@decky/ui` 4,
     `@decky/api` 1, `@decky/rollup`; npm instead of pnpm). Zip via
     `scripts/package-decky.sh` → `dist/yasgm-decky-<version>.zip` +
     `.sha256`, layout `yasgm/{dist/index.js, main.py, plugin.json,
     package.json, LICENSE, README.md, bin/yasgm-x86_64, bin/yasgm-aarch64}`
     (4.4 MB). No `root` flag: runs as the Decky user, so it shares
     config/state/lock with desktop-mode YASGM (`HOME` forced to
     `DECKY_USER_HOME`, `LD_LIBRARY_PATH` dropped).
   - Backend `main.py`: picks `bin/yasgm-<arch>` by `platform.machine()`
     and restores its exec bit (zip extraction drops it — observed);
     supervises `yasgm watch` as a child (log
     `DECKY_PLUGIN_LOG_DIR/daemon.log`), restarting with backoff 5 s→5 min;
     exit 3 (another daemon holds the lock, e.g. desktop autostart) →
     uses that one and re-checks every 60 s. `_unload`: `watch send quit`,
     then terminate/kill. Callables: `get_status` (status --json + watch
     send status), `get_games`, `get_versions`, `sync_now`, `set_paused`,
     `game_event` (→ `watch send game-started|game-stopped`), `restore`,
     `set_pinned`, `set_mode`, `use_onedrive`, `start_sign_in` /
     `cancel_sign_in` (streams `auth --device --json` lines to the
     frontend as `yasgm_auth` events). Daemon ignores start/stop hints
     for apps that aren't installed Steam games (non-Steam shortcuts).
   - Frontend: QAM panel — status (provider/connection, daemon state,
     pending games), Sync now, Pause toggle; OneDrive sign-in section
     (QR of the verification URL via `qrcode-generator` + large user
     code) when not signed in; "Use OneDrive instead" for the LocalFolder
     provider; games list (mode · last synced) → route
     `/yasgm/game/:appId` with mode dropdown, Sync this game, versions
     with Restore (ConfirmModal; current saves kept as pinned first) and
     Pin/Unpin, cross-OS note. `RegisterForAppLifetimeNotifications` →
     `game_event`; after a stop it re-reads `games --json` at +20 s/+60 s
     and toasts "Saves synced — <game>" if `synced_at` moved (no
     continuous polling: `games --json` parses the 17 MB manifest).
   - Verified on the steamac VM with the real zip and a stand-in `decky`
     module (constants/logger/emit): every callable; pause/hints visible
     in daemon status; unknown-app hint ignored; sign-in relay
     (`not_needed` on LocalFolder); existing-daemon case (exit 3 → uses
     it); plugin-owned daemon start, `kill -9` → restarted in 5 s, clean
     unload with socket removed.
   - Not done yet: conflict toast (D5), per-game keep setting,
     "pull newer saves before launch" (decky-cloud-save pauses the game
     with SIGSTOP meanwhile — candidate).
5. **Decky self-update** (D21): release zip + sha256 + signature, in-plugin
   update check/apply/rollback.
6. **SteamOS validation** — first in the **steamac VM** on the dev Mac (see
   [Test environment](#test-environment-steamac-vm)), then real Deck
   hardware for what the VM can't cover (carry-over from Phase 2):
   Proton prefix end-to-end, Windows↔Deck cross-restore, Gaming Mode
   sign-in, sleep/resume mid-sync, self-update/rollback.
7. **Release pipeline** (GitHub Actions): tag → macOS + Windows bundles via
   `tauri-action` with updater signatures + `latest.json`, Decky zips per
   arch; GitHub Release. Version single-sourced (root `Cargo.toml`, `ui`
   crate, `tauri.conf.json`, Decky `package.json`).
8. Docs: README install/update instructions per platform, PCGamingWiki
   attribution (D10), privacy note (data only in the user's own cloud).
9. **macOS signing + notarization (D16)** — last: add Developer ID
   identity + notarization API key to the build, verify Gatekeeper accepts
   the downloaded `.dmg` and that a signed→signed auto-update works.

Explicitly **post-1.0**: Flathub desktop GUI on SteamOS, Linux desktop
tray, extra cloud providers, GOG/Epic libraries (D3), Windows code signing,
package-manager listings, Decky store submission (not planned, D21).

### Test environment: steamac VM

`fxgl/steamac` (installed on the dev Mac) runs Valve's official **ARM64
SteamOS** (the Steam Frame image, `VARIANT_ID=steamdeck`) in a libkrun VM on
Apple Silicon, with Gaming Mode, Desktop Mode, Steam, and Proton 11 ARM64 +
FEX for x86 games. SSH: `ssh -p 2222 steamos@127.0.0.1` (dev launcher;
enable in Settings → Advanced in the `.app`). Decky Loader's installer
supports it (`uname -m` → `PluginLoader-arm64` asset, user detected via
`SUDO_USER`).

Good for: installing Decky + sideloading our plugin, the QAM UI in real
Gaming Mode, QR device-code sign-in, daemon lifecycle under
`plugin_loader`, self-update/rollback, Steam app-lifetime notifications,
real Proton `compatdata/<appid>/pfx/drive_c/users/steamuser` save paths,
cross-restore Mac/Windows ↔ SteamOS, sleep/resume of the VM.

Differences from a real Deck — so the VM does not replace one final
on-device check:
- **aarch64, not x86_64** → CI must also build
  `aarch64-unknown-linux-musl` (cheap with musl; the plugin picks the
  binary by `uname -m`). The x86_64 build itself is only exercised on a
  real Deck.
- User is **`steamos`**, home `/home/steamos` — not `deck`. Useful: catches
  any hard-coded `/home/deck`.
- No SD-card library, no real Deck controller/suspend hardware, Frame-image
  service masks.


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
- **Decky store acceptance** (AI-authorship attestation, human review
  latency). Mitigation: off-store zip + in-plugin self-update fallback.
- **Updater key loss** bricks auto-update for all existing installs.
  Mitigation: secrets + offline backup before the first public release.
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
- https://github.com/SteamDeckHomebrew/decky-loader (store.tsx, plugin-loader.tsx, updater.py, browser.py)
- https://github.com/SteamDeckHomebrew/decky-plugin-database (submodule model, PR templates)
- https://github.com/SteamDeckHomebrew/decky-plugin-template
- https://github.com/SteamDeckHomebrew/wiki/blob/main/plugin-dev/review-and-testing.md
- https://github.com/GedasFX/decky-ludusavi, https://github.com/GedasFX/decky-cloud-save
- https://v2.tauri.app/plugin/updater/
