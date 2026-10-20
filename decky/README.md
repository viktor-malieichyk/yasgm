# YASGM for Decky Loader

Game save sync and backup in Steam Deck Gaming Mode, to your own OneDrive
(or a folder). The plugin runs YASGM's sync daemon (`bin/yasgm-<arch>`, a
static build of the same CLI used on Windows and macOS) and shows its state
in the Quick Access menu:

- status, Sync now, Pause
- OneDrive sign-in with a phone (QR code + device code)
- per-game mode and saved versions, with Restore and Pin

Steam's own game start/stop notifications are forwarded to the daemon, so
saves are synced right after you quit a game. Daemon log:
`~/homebrew/logs/yasgm/daemon.log`.

Build: `scripts/package-decky.sh` in the YASGM repo → `dist/yasgm-decky-<version>.zip`.
