# YASGM — Yet another save game manager

Seamless, setup-free cloud sync for PC game saves, using your own cloud
storage. The only setup step is signing in to a cloud service — save
locations for 50,000+ games are discovered automatically.

Runs on **Windows, macOS and Steam Deck / SteamOS** (as a Decky plugin in
Gaming Mode), and they all share one cloud: play on the PC, continue on the
Deck.

> **Status: pre-1.0.** The engine is verified against a real OneDrive and
> Steam library on macOS, Windows and SteamOS. macOS builds are not yet
> signed by Apple and the Windows installer isn't code-signed — see the
> install notes. [DESIGN.md](DESIGN.md) has the design, decision log and
> roadmap.

## Install

Downloads are on the [Releases page](https://github.com/viktor-malieichyk/yasgm/releases/latest).

### Windows

1. Download `YASGM_<version>_x64-setup.exe` and run it. Windows SmartScreen
   may warn about an unknown publisher (the installer isn't code-signed yet):
   **More info → Run anyway**.
2. Open YASGM, go to **Settings → Cloud sync** and sign in (with a browser,
   or by scanning a QR code with your phone).
3. Background sync runs from the tray icon; turn on **Settings → Autostart**
   ("Launch YASGM's sync daemon at login") to keep it running.

### macOS

1. Download the `.dmg` for your Mac (`aarch64` = Apple Silicon, `x64` =
   Intel) and drag YASGM to Applications.
2. The app isn't signed by Apple yet, so the first launch is blocked:
   right-click YASGM → **Open** → **Open** (or System Settings → Privacy &
   Security → **Open Anyway**).
3. Sign in under **Settings → Cloud sync**, as on Windows. The menu-bar icon
   runs background sync.

### Steam Deck / SteamOS (Decky plugin)

1. Install [Decky Loader](https://decky.xyz) (Desktop Mode; it needs a sudo
   password — set one with `passwd` in Konsole if you haven't).
2. Download `yasgm-decky-<version>.zip` from the release onto the Deck.
3. In Gaming Mode: **… → Decky → ⚙ → General → Developer mode** on, then
   **Developer → Install Plugin from ZIP File** and pick the zip.
4. Open **… → Decky → YASGM** and **Sign in with a phone**: scan the QR code
   and enter the code shown.

The plugin runs the sync daemon itself — nothing to start by hand. Saves
sync when you quit a game, and newer saves from your other devices are
pulled in before a game starts. Toasts tell you what happened.

### Updates

- **Windows / macOS:** the app checks for updates on start and daily
  (Settings → Updates) and offers **Install & restart**. Updates are signed
  and verified before they're installed.
- **Steam Deck:** the plugin checks GitHub twice a day and shows **Update to
  <version>** in its panel. It installs only releases signed with the
  project's release key (checked on the Deck before installing); Decky asks
  you to confirm and reloads the plugin. **About → Reinstall <previous>**
  rolls back.

## Privacy

- Your saves go only to **your own** cloud storage. There is no YASGM server
  and no account, analytics or telemetry.
- With OneDrive, YASGM uses Microsoft's app-folder permission: it can only
  see its own `Apps/YASGM/` folder, nothing else in your drive. Sign-in
  tokens stay on your device. **Disconnect** (Settings, or the Deck panel)
  deletes them; revoke the app at
  [account.live.com/consent/Manage](https://account.live.com/consent/Manage).
- Network requests besides your cloud: the save-location database
  (GitHub), game icons (Steam's public store API/CDN) and update checks
  (GitHub Releases).

## How it works

- **Game discovery** — installed Steam games come from your Steam library;
  save locations from the community-maintained
  [Ludusavi Manifest](https://github.com/mtkennerly/ludusavi-manifest)
  (compiled from [PCGamingWiki](https://www.pcgamingwiki.com/)). No per-game
  configuration.
- **Versioned snapshots** — every sync stores an immutable snapshot; the
  last 10 versions per game are kept (configurable per game). Restores are
  always reversible: the current state is preserved first.
- **Non-destructive by design** — YASGM never overwrites local save files
  without capturing them in a snapshot first. Conflicts keep both sides:
  the newer becomes active, the other stays pinned until you delete it.
- **Steam Cloud aware** — games Steam Cloud already syncs get backup-only
  mode (snapshots + manual restore), so the two never fight.
- **Cross-OS saves** — snapshots record path *templates*, not absolute
  paths, so a save made on Windows restores into a Proton prefix on SteamOS
  and vice versa. Games whose save folders differ too much between systems
  are flagged instead of guessed.

## Command line

The desktop apps and the Deck plugin drive a CLI, `yasgm`, which also works
on its own (e.g. on a Linux desktop):

```
yasgm auth [--device]      # cloud sign-in (--device: code + QR for another device)
yasgm doctor               # games found, where their saves are, what mode they get
yasgm sync [appid]         # two-way sync (upload / download / conflict-keep-both)
yasgm watch                # daemon: sync when saves change or a game exits
yasgm run [--app <id>] -- <command>   # launch wrapper: sync, run game, sync
yasgm backup [appid]       # snapshot + upload only
yasgm versions [appid]     # list cloud versions
yasgm restore <appid> [--version <id>]
yasgm config [<appid> --mode auto|sync|backup|off --keep N|default]
yasgm pin|unpin|rm <appid> <version-id>
yasgm provider onedrive|local <folder>|disconnect|status
yasgm cloud-games [--json]   # games with cloud saves that aren't installed here
yasgm move-account <from> [--to <account>] [--dry-run] [--keep-source]
                             # move saves uploaded under another Steam account
```

State-changing commands accept `--dry-run`. For guaranteed sync around a
specific game, set its Steam launch options to `yasgm run -- %command%`.

## Building

- CLI: Rust (stable) — `cargo build --release`.
- Desktop app: plus Node — `cd ui && npm ci && npm run tauri build`.
- Deck plugin: `scripts/package-decky.sh` (needs `zig` and `cargo-zigbuild`
  for the static Linux builds) → `dist/yasgm-decky-<version>.zip`.
- Windows installer from a Mac: `scripts/build-windows.sh`.

### Releasing

```
scripts/version.sh 0.2.0          # sets the version everywhere
git commit -am "Release 0.2.0" && git tag v0.2.0 && git push --follow-tags
```

The tag starts `.github/workflows/release.yml`: macOS and Windows bundles
with updater signatures and `latest.json`, plus the Decky zip, published
together as one GitHub Release once every build has finished. Needs the
`TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
repository secrets.

## Acknowledgements

- Save location data comes from the
  [Ludusavi Manifest](https://github.com/mtkennerly/ludusavi-manifest) (MIT),
  which compiles data contributed by the
  [PCGamingWiki](https://www.pcgamingwiki.com/) community. If you find a
  missing or wrong save path, please fix it on the wiki — every tool using
  the manifest benefits.
- [Ludusavi](https://github.com/mtkennerly/ludusavi) pioneered much of this
  approach and is a great backup-focused alternative.

## License

[MIT](LICENSE)
