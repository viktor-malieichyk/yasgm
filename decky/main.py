"""YASGM Decky plugin backend.

Runs the bundled static `yasgm` binary (bin/yasgm-<arch>) as the save-sync
daemon (`yasgm watch`, a child of this backend) and answers the frontend by
shelling out to `yasgm … --json` — the same subprocess + JSON seam the
desktop GUI uses. Runs as the Decky user (no root flag), so the daemon
shares its config, state and lock with a desktop-mode YASGM.
"""

import asyncio
import json
import os
import platform
import signal
import stat

import decky

BIN_DIR = os.path.join(decky.DECKY_PLUGIN_DIR, "bin")
DAEMON_LOG = os.path.join(decky.DECKY_PLUGIN_LOG_DIR, "daemon.log")
# `yasgm watch` exits with this when another daemon already holds the lock
# (e.g. one started from desktop mode); that one serves us just as well.
ALREADY_RUNNING = 3


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


class Plugin:
    daemon = None  # asyncio.subprocess.Process of our `yasgm watch` child
    stopping = False
    sign_in = None  # running `yasgm auth --device --json`

    # ---- daemon lifecycle -------------------------------------------------

    async def _main(self):
        self.stopping = False
        self.loop = asyncio.get_event_loop()
        self.loop.create_task(self._supervise_daemon())
        self.loop.create_task(self._relay_sync_events())

    async def _relay_sync_events(self):
        """Polls the daemon's event log and emits each new sync event to the
        frontend (`yasgm_sync_event`), which shows it as a toast."""
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
            if since is not None:
                for event in reply.get("events", []):
                    await decky.emit("yasgm_sync_event", event)
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

    async def get_status(self) -> dict:
        """`status --json` plus the daemon's own state when it's reachable."""
        status = await run_json("status", "--json", timeout=60)
        if status.get("daemon", {}).get("running"):
            try:
                status["watch"] = json.loads(await run("watch", "send", "status", timeout=15))
            except Exception as err:
                status["watch_error"] = str(err)
        return status

    async def get_games(self) -> list:
        return await run_json("games", "--json")

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
        signal_all(frozen, signal.SIGSTOP)
        try:
            reply = await ask_daemon(f"game-started {app_id}", timeout=170)
            if reply != "ok":
                decky.logger.warning(f"pre-launch sync of {app_id}: {reply}")
        except Exception as err:
            decky.logger.warning(f"could not forward game start for {app_id}: {err}")
        finally:
            signal_all(frozen, signal.SIGCONT)
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

    async def use_onedrive(self) -> str:
        return (await run("provider", "onedrive")).strip()

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
