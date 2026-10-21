"""YASGM Decky plugin backend.

Runs the bundled static `yasgm` binary (bin/yasgm-<arch>) as the save-sync
daemon (`yasgm watch`, a child of this backend) and answers the frontend by
shelling out to `yasgm … --json` — the same subprocess + JSON seam the
desktop GUI uses. Runs as the Decky user (no root flag), so the daemon
shares its config, state and lock with a desktop-mode YASGM.
"""

import asyncio
import hashlib
import json
import os
import platform
import re
import signal
import ssl
import stat
import time
import urllib.request

import decky

BIN_DIR = os.path.join(decky.DECKY_PLUGIN_DIR, "bin")
DAEMON_LOG = os.path.join(decky.DECKY_PLUGIN_LOG_DIR, "daemon.log")
# Processes paused for a pre-launch sync, so a backend that dies (crash,
# Decky restart, plugin update mid-launch) before resuming them doesn't
# leave the game frozen: the next start resumes whatever is listed here.
FROZEN_FILE = os.path.join(getattr(decky, "DECKY_PLUGIN_RUNTIME_DIR", decky.DECKY_PLUGIN_LOG_DIR), "frozen.json")
# `yasgm watch` exits with this when another daemon already holds the lock
# (e.g. one started from desktop mode); that one serves us just as well.
ALREADY_RUNNING = 3

# Self-update (D21): releases of this repo carrying a `yasgm-decky-<ver>.zip`.
RELEASES_API = "https://api.github.com/repos/viktor-malieichyk/yasgm/releases?per_page=30"
PLUGIN_ZIP = re.compile(r"^yasgm-decky-(.+)\.zip$")
UPDATE_CACHE_SECS = 6 * 3600
UPDATE_CHECK_EVERY = 12 * 3600


def yasgm_path() -> str:
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(platform.machine().lower(), platform.machine().lower())
    path = os.path.join(BIN_DIR, f"yasgm-{arch}")
    if not os.path.exists(path):
        raise RuntimeError(f"no yasgm binary for {arch} in {BIN_DIR}")
    # Zip extraction can drop the executable bit.
    mode = os.stat(path).st_mode
    if not mode & stat.S_IXUSR:
        os.chmod(path, mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


def yasgm_env() -> dict:
    env = dict(os.environ)
    env["HOME"] = decky.DECKY_USER_HOME
    # Decky's bundled Python sets this for itself; yasgm is static anyway.
    env.pop("LD_LIBRARY_PATH", None)
    return env


async def run(*args: str, timeout: float = 120) -> str:
    """Runs `yasgm args…`; returns stdout, raises with stderr on failure."""
    proc = await asyncio.create_subprocess_exec(
        yasgm_path(), *args,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=yasgm_env(),
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(), timeout)
    except asyncio.TimeoutError:
        proc.kill()
        raise RuntimeError(f"yasgm {' '.join(args)} timed out")
    if proc.returncode != 0:
        message = err.decode(errors="replace").strip() or out.decode(errors="replace").strip()
        raise RuntimeError(message or f"yasgm {args[0]} failed ({proc.returncode})")
    return out.decode(errors="replace")


async def run_json(*args: str, timeout: float = 120):
    return json.loads(await run(*args, timeout=timeout))


def socket_path() -> str:
    """The daemon's control socket (`yasgm watch send` talks to it too)."""
    data = os.environ.get("XDG_DATA_HOME")
    if not data or not os.path.isabs(data):
        data = os.path.join(decky.DECKY_USER_HOME, ".local", "share")
    return os.path.join(data, "yasgm", "watch.sock")


async def ask_daemon(request: str, timeout: float = 10) -> str:
    """One request over the control socket, without starting a process."""
    reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(socket_path()), timeout)
    try:
        writer.write(request.encode() + b"\n")
        await writer.drain()
        return (await asyncio.wait_for(reader.readline(), timeout)).decode().strip()
    finally:
        writer.close()


def process_tree(root: int) -> list:
    """`root` and all its descendants that belong to us (from /proc)."""
    children = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                # pid (comm) state ppid …; comm may contain spaces/parens.
                ppid = int(f.read().rsplit(")", 1)[1].split()[1])
        except (OSError, ValueError, IndexError):
            continue
        children.setdefault(ppid, []).append(int(entry))
    tree, todo = [], [root]
    while todo:
        pid = todo.pop()
        try:
            if os.stat(f"/proc/{pid}").st_uid != os.getuid():
                continue
        except OSError:
            continue
        tree.append(pid)
        todo.extend(children.get(pid, []))
    return tree


def signal_all(pids: list, sig) -> None:
    for pid in pids:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


def remember_frozen(pids: list) -> None:
    try:
        if pids:
            with open(FROZEN_FILE, "w") as f:
                json.dump(pids, f)
        elif os.path.exists(FROZEN_FILE):
            os.remove(FROZEN_FILE)
    except OSError as err:
        decky.logger.warning(f"could not record paused processes: {err}")


def resume_leftover_frozen() -> None:
    """Resumes processes an earlier run paused and never resumed. SIGCONT
    to a process that isn't stopped (or a reused pid of ours) is harmless."""
    try:
        with open(FROZEN_FILE) as f:
            pids = [int(p) for p in json.load(f)]
    except (OSError, ValueError, TypeError):
        return
    # The kernel only lets us signal our own processes.
    signal_all(pids, signal.SIGCONT)
    remember_frozen([])
    if pids:
        decky.logger.warning(f"resumed {len(pids)} process(es) an earlier run left paused")


def plugin_version() -> str:
    version = getattr(decky, "DECKY_PLUGIN_VERSION", None)
    if not version:
        with open(os.path.join(decky.DECKY_PLUGIN_DIR, "package.json")) as f:
            version = json.load(f)["version"]
    return str(version)


def version_key(version: str) -> tuple:
    """"v1.2.10" → (1, 2, 10); anything after a "-" (pre-release) sorts lower."""
    core, _, pre = version.lstrip("v").partition("-")
    parts = tuple(int(p) if p.isdigit() else 0 for p in core.split("."))
    return parts + ((0,) if pre else (1,))


def ssl_context() -> ssl.SSLContext:
    """Decky's bundled Python may not find the system CA store by itself."""
    try:
        import certifi

        return ssl.create_default_context(cafile=certifi.where())
    except Exception:
        pass
    for cafile in ("/etc/ssl/certs/ca-certificates.crt", "/etc/ca-certificates/extracted/tls-ca-bundle.pem"):
        if os.path.exists(cafile):
            return ssl.create_default_context(cafile=cafile)
    return ssl.create_default_context()


def http_get(url: str, accept: str = "application/vnd.github+json") -> bytes:
    request = urllib.request.Request(url, headers={"Accept": accept, "User-Agent": "yasgm-decky"})
    with urllib.request.urlopen(request, timeout=20, context=ssl_context()) as response:
        return response.read()


def plugin_releases() -> list:
    """Published (non-draft, non-prerelease) releases with a plugin zip and
    its signature, newest first: [{version, url, sig, notes}]."""
    found = []
    for release in json.loads(http_get(RELEASES_API)):
        if release.get("draft") or release.get("prerelease"):
            continue
        assets = release.get("assets", [])
        for asset in assets:
            match = PLUGIN_ZIP.match(asset.get("name", ""))
            if not match:
                continue
            sig = next((a["browser_download_url"] for a in assets if a.get("name") == asset["name"] + ".sig"), None)
            if sig:  # unsigned releases can't be verified, so aren't offered
                found.append({
                    "version": match.group(1),
                    "url": asset["browser_download_url"],
                    "sig": sig,
                    "notes": (release.get("body") or "").strip(),
                })
            break
    found.sort(key=lambda r: version_key(r["version"]), reverse=True)
    return found


def update_info() -> dict:
    """What the panel needs: the newest release (if newer than this plugin)
    and the newest older one (to roll back to), each with url + hash."""
    current = plugin_version()
    releases = plugin_releases()
    newer = [r for r in releases if version_key(r["version"]) > version_key(current)]
    older = [r for r in releases if version_key(r["version"]) < version_key(current)]
    pick = lambda r: r and {"version": r["version"], "url": r["url"], "sig": r["sig"], "notes": r["notes"][:600]}
    return {
        "current": current,
        "update": pick(newer[0] if newer else None),
        "previous": pick(older[0] if older else None),
        "checked_at": int(time.time()),
    }


class Plugin:
    daemon = None  # asyncio.subprocess.Process of our `yasgm watch` child
    stopping = False
    sign_in = None  # running `yasgm auth --device --json`
    updates = None  # last update_info()
    announced = None  # update version already toasted
    # Slow CLI answers (manifest parse, cloud requests), kept so the panel
    # opens instantly: the cached copy first, a fresh one right after.
    cache = None
    refreshing = None

    # ---- daemon lifecycle -------------------------------------------------

    async def _main(self):
        resume_leftover_frozen()
        self.stopping = False
        self.loop = asyncio.get_event_loop()
        self.loop.create_task(self._supervise_daemon())
        self.loop.create_task(self._relay_sync_events())
        self.loop.create_task(self._watch_for_updates())
        self.cache = {}
        self.refreshing = {}
        # Warm the caches the panel needs, so opening it the first time is quick.
        for name in ("games", "icons", "status", "cloud_games"):
            self.loop.create_task(self._refresh(name))

    _QUERIES = {
        "games": (("games", "--json"), 120),
        "icons": (("icons", "--json", "--embed"), 120),
        "status": (("status", "--json"), 60),
        "cloud_games": (("cloud-games", "--json"), 120),
    }

    async def _refresh(self, name: str):
        """Runs one slow query and caches its result; concurrent callers
        share the same run."""
        running = self.refreshing.get(name)
        if running is None or running.done():
            args, timeout = self._QUERIES[name]
            running = self.loop.create_task(run_json(*args, timeout=timeout))
            self.refreshing[name] = running
        try:
            self.cache[name] = await asyncio.shield(running)
        except Exception as err:
            if name not in self.cache:
                raise
            decky.logger.info(f"refreshing {name} failed, keeping the cached copy: {err}")
        return self.cache[name]

    async def _cached(self, name: str, fresh: bool):
        if not fresh and name in self.cache:
            return self.cache[name]
        return await self._refresh(name)

    async def _watch_for_updates(self):
        """Checks GitHub for a newer plugin release shortly after start and
        then twice a day; a new one is announced once (`yasgm_update`)."""
        await asyncio.sleep(60)
        while not self.stopping:
            try:
                info = await self.check_update(True)
                update = info.get("update")
                if update and update["version"] != self.announced:
                    self.announced = update["version"]
                    await decky.emit("yasgm_update", update)
            except Exception as err:
                decky.logger.info(f"update check failed: {err}")
            await asyncio.sleep(UPDATE_CHECK_EVERY)

    async def _relay_sync_events(self):
        """Polls the daemon's event log and emits new sync events to the
        frontend (`yasgm_sync_events`, a list), which shows them as toasts."""
        since = None
        while not self.stopping:
            await asyncio.sleep(5)
            try:
                reply = json.loads(await ask_daemon("events" if since is None else f"events {since}"))
            except Exception:
                continue  # daemon not running (yet) or busy syncing
            seq = reply.get("seq", 0)
            if since is not None and seq < since:
                since = None  # daemon restarted: start over from its count
                continue
            if since is not None and reply.get("events"):
                # One batch per poll, so the frontend can merge a burst (e.g.
                # the daemon's first pass) into a single toast.
                await decky.emit("yasgm_sync_events", reply["events"])
                # Last-synced times and local saves changed.
                self.loop.create_task(self._refresh("games"))
            since = seq

    async def _supervise_daemon(self):
        """Keeps `yasgm watch` running; restarts it (with backoff) if it dies."""
        delay = 5
        while not self.stopping:
            try:
                with open(DAEMON_LOG, "ab") as log:
                    self.daemon = await asyncio.create_subprocess_exec(
                        yasgm_path(), "watch",
                        stdin=asyncio.subprocess.DEVNULL, stdout=log, stderr=log, env=yasgm_env(),
                    )
                decky.logger.info(f"started yasgm watch (pid {self.daemon.pid})")
                code = await self.daemon.wait()
            except Exception as err:  # binary missing, spawn failure, …
                decky.logger.error(f"could not start yasgm watch: {err}")
                code = None
            self.daemon = None
            if self.stopping:
                break
            if code == ALREADY_RUNNING:
                # Another daemon serves us; check back later in case it stops.
                decky.logger.info("another yasgm watch daemon is running; using it")
                await asyncio.sleep(60)
                continue
            decky.logger.warning(f"yasgm watch exited ({code}); restarting in {delay}s")
            await asyncio.sleep(delay)
            delay = min(delay * 2, 300)

    async def _unload(self):
        self.stopping = True
        await self.cancel_sign_in()
        daemon = self.daemon
        if daemon is None or daemon.returncode is not None:
            return
        # Ask for a clean quit (finishes a sync in progress), then insist.
        try:
            await run("watch", "send", "quit", timeout=10)
            await asyncio.wait_for(daemon.wait(), 30)
        except Exception:
            daemon.terminate()
            try:
                await asyncio.wait_for(daemon.wait(), 10)
            except asyncio.TimeoutError:
                daemon.kill()
        decky.logger.info("yasgm watch stopped")

    async def _uninstall(self):
        pass

    # ---- queries ----------------------------------------------------------

    async def get_status(self, fresh: bool = False) -> dict:
        """`status --json` (provider + connection check; the daemon's live
        state comes from `get_daemon_state`)."""
        return await self._cached("status", fresh)

    async def get_daemon_state(self) -> dict:
        """The daemon's live state straight from its socket — cheap enough to
        poll every couple of seconds: {running, paused, syncing, syncing_game,
        running_game, pending_sync, watching_games} or {running: False}."""
        try:
            state = json.loads(await ask_daemon("status", timeout=3))
        except Exception as err:
            return {"running": False, "error": str(err), "starting": self.daemon is not None}
        state["running"] = True
        return state

    async def get_games(self, fresh: bool = False) -> list:
        return await self._cached("games", fresh)

    async def get_cloud_games(self, fresh: bool = False) -> list:
        """Games with cloud saves that aren't installed on this Deck."""
        if not fresh and "cloud_games" not in self.cache:
            # Before the first cloud answer: yasgm's own cached copy.
            try:
                return await run_json("cloud-games", "--cached", timeout=15)
            except Exception:
                return []
        return await self._cached("cloud_games", fresh)

    async def get_icons(self, fresh: bool = False) -> dict:
        """appid → data: URL of the game's icon (Steam's cache, else
        downloaded once into yasgm's cache)."""
        icons = await self._cached("icons", fresh)
        return {str(i["app_id"]): i["data_url"] for i in icons if i.get("data_url")}

    async def check_update(self, force: bool = False) -> dict:
        """Newest plugin release on GitHub vs. this one (cached 6 h)."""
        if force or not self.updates or time.time() - self.updates["checked_at"] > UPDATE_CACHE_SECS:
            self.updates = await asyncio.to_thread(update_info)
        return self.updates

    async def prepare_install(self, version: str) -> dict:
        """Downloads the release zip offered for `version` and checks its
        signature with the bundled yasgm (release key, D21); returns the
        URL and the SHA-256 of exactly the verified bytes. Decky downloads
        the zip again and refuses it unless that hash matches, so what gets
        installed is what was verified."""
        info = self.updates or {}
        release = next((r for r in (info.get("update"), info.get("previous")) if r and r["version"] == version), None)
        if release is None:
            raise RuntimeError(f"no release {version} offered; check for updates first")
        data, sig = await asyncio.to_thread(
            lambda: (http_get(release["url"], accept="application/octet-stream"), http_get(release["sig"], accept="application/octet-stream"))
        )
        work = os.path.join(getattr(decky, "DECKY_PLUGIN_RUNTIME_DIR", decky.DECKY_PLUGIN_LOG_DIR), "update")
        os.makedirs(work, exist_ok=True)
        zip_path, sig_path = os.path.join(work, "plugin.zip"), os.path.join(work, "plugin.zip.sig")
        with open(zip_path, "wb") as f:
            f.write(data)
        with open(sig_path, "wb") as f:
            f.write(sig)
        try:
            await run("verify-update", zip_path, sig_path, timeout=60)
        finally:
            for path in (zip_path, sig_path):
                try:
                    os.remove(path)
                except OSError:
                    pass
        return {"version": version, "url": release["url"], "hash": hashlib.sha256(data).hexdigest()}

    async def get_versions(self, app_id: int) -> list:
        return await run_json("versions", str(app_id), "--json")

    # ---- actions ----------------------------------------------------------

    async def sync_now(self, app_id: int = 0) -> str:
        args = ["watch", "send", "sync"] + ([str(app_id)] if app_id else [])
        return (await run(*args, timeout=15)).strip()

    async def set_paused(self, paused: bool) -> str:
        return (await run("watch", "send", "pause" if paused else "resume", timeout=15)).strip()

    async def game_event(self, app_id: int, running: bool, pid: int = 0) -> None:
        """Forwards Steam's app-lifetime notification to the daemon. On a
        start, the game (`pid`, Steam's nInstanceID, and its descendants) is
        paused while the daemon syncs it, so newer cloud saves land before
        the game reads them; it's always resumed afterwards."""
        if not running:
            try:
                await ask_daemon(f"game-stopped {app_id}", timeout=15)
            except Exception as err:
                decky.logger.warning(f"could not forward game stop for {app_id}: {err}")
            return
        frozen = process_tree(pid) if pid > 1 else []
        remember_frozen(frozen)
        signal_all(frozen, signal.SIGSTOP)
        try:
            reply = await ask_daemon(f"game-started {app_id}", timeout=170)
            if reply != "ok":
                decky.logger.warning(f"pre-launch sync of {app_id}: {reply}")
        except Exception as err:
            decky.logger.warning(f"could not forward game start for {app_id}: {err}")
        finally:
            signal_all(frozen, signal.SIGCONT)
            remember_frozen([])
        if frozen:
            decky.logger.info(f"held {app_id} ({len(frozen)} processes) for its pre-launch sync")

    async def restore(self, app_id: int, version_id: str) -> str:
        return (await run("restore", str(app_id), "--version", version_id, timeout=600)).strip()

    async def set_pinned(self, app_id: int, version_id: str, pinned: bool) -> str:
        return (await run("pin" if pinned else "unpin", str(app_id), version_id)).strip()

    async def set_keep(self, app_id: int, keep: int) -> str:
        """How many versions to keep for a game; 0 = the default."""
        return (await run("config", str(app_id), "--keep", str(keep) if keep > 0 else "default")).strip()

    async def set_mode(self, app_id: int, mode: str) -> str:
        if mode not in ("auto", "sync", "backup", "off"):
            raise ValueError(f"unknown mode {mode}")
        return (await run("config", str(app_id), "--mode", mode)).strip()

    async def disconnect(self) -> str:
        """Signs out of OneDrive on this Deck (or stops using the folder)."""
        await self.cancel_sign_in()
        return (await run("provider", "disconnect")).strip()

    # ---- OneDrive sign-in (device code) ------------------------------------

    async def start_sign_in(self) -> None:
        """Starts `auth --device --json`; each JSON line is emitted to the
        frontend as a `yasgm_auth` event (code → waiting… → signed_in/error)."""
        await self.cancel_sign_in()
        self.sign_in = await asyncio.create_subprocess_exec(
            yasgm_path(), "auth", "--device", "--json",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=yasgm_env(),
        )
        self.loop.create_task(self._relay_sign_in(self.sign_in))

    async def _relay_sign_in(self, proc):
        finished = False
        async for raw in proc.stdout:
            try:
                event = json.loads(raw)
            except ValueError:
                continue
            finished = finished or event.get("event") in ("signed_in", "error", "not_needed")
            await decky.emit("yasgm_auth", event)
        await proc.wait()
        if not finished and proc.returncode != 0 and self.sign_in is proc:
            await decky.emit("yasgm_auth", {"event": "error", "message": "sign-in stopped"})
        if self.sign_in is proc:
            self.sign_in = None

    async def cancel_sign_in(self) -> None:
        proc, self.sign_in = self.sign_in, None
        if proc is not None and proc.returncode is None:
            proc.terminate()
