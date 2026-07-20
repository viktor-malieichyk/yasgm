# YASGM — Yet another save game manager

Seamless, setup-free cloud sync for PC game saves, using your own cloud
storage. The only setup step is authorizing a cloud service — save locations
for 50,000+ games are discovered automatically.

> **Status: early development.** The core engine works (verified against a
> real OneDrive and Steam library on macOS and Windows), but it has not
> shipped a release yet. SteamOS validation is in progress. See
> [DESIGN.md](DESIGN.md) for the full design, decision log, and roadmap.

## How it works

- **Game discovery** — your installed Steam games are enumerated from the
  Steam library; save locations come from the community-maintained
  [Ludusavi Manifest](https://github.com/mtkennerly/ludusavi-manifest)
  (compiled from [PCGamingWiki](https://www.pcgamingwiki.com/)). No per-game
  configuration.
- **Cloud storage** — saves go to *your* OneDrive, into a sandboxed
  `Apps/YASGM/` folder. YASGM uses the Microsoft Graph app-folder scope and
  cannot see anything else in your drive. More providers are planned behind
  the same interface.
- **Versioned snapshots** — every sync stores an immutable snapshot; the
  last 10 versions per game are kept (configurable). Restores are always
  reversible: the current state is preserved first.
- **Non-destructive by design** — YASGM never deletes or overwrites local
  save files without capturing them in a snapshot first. Conflicts keep
  both sides: the newer becomes active, the other stays pinned until you
  delete it yourself.
- **Steam Cloud aware** — games already synced by Steam Cloud are
  auto-detected and get backup-only mode (snapshots + manual restore) so
  the two sync systems never fight.
- **Cross-OS saves** — snapshots record path *templates*, not absolute
  paths, so a save captured on Windows restores into a Proton prefix on
  SteamOS (and vice versa).

## Usage

```
yasgm auth [--device]      # one-time cloud sign-in (device flow for Steam Deck)
yasgm doctor               # what games were found, where their saves are, what mode they get
yasgm sync [appid]         # bidirectional sync (upload / download / conflict-keep-both)
yasgm run [--app <id>] -- <command>   # launch wrapper: sync, run game, sync
yasgm backup [appid]       # snapshot + upload only
yasgm versions [appid]     # list cloud versions
yasgm restore <appid> [--version <id>]
yasgm config [<appid> --mode auto|sync|backup|off --keep N]
yasgm pin|unpin|rm <appid> <version-id>
```

All state-changing commands accept `--dry-run`.

### Steam integration (per game, optional)

For guaranteed sync-before-play and sync-after-exit, set the game's Steam
launch options (right-click the game → Properties → Launch Options) to:

```
yasgm run -- %command%
```

The game's AppID is picked up automatically from Steam's environment. A
failed sync never blocks the game from launching — worst case you play on
local saves and the next sync reconciles (keeping both sides on conflict).

## Building

Rust (stable) is the only requirement:

```
cargo build --release
./target/release/yasgm doctor
```

Supported platforms: Windows 10+, SteamOS/Linux, macOS.

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
