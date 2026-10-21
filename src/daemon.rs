//! The watch daemon (`yasgm watch`): file watching, game start/stop
//! detection, periodic cloud checks, the control socket's shared state, and
//! `watch status|start|stop|send`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::args::*;
use crate::config::{self, ModeOverride};
use crate::commands::sync::{SyncEvent, run_sync};
use crate::ctx::*;
use crate::resolve;
use crate::store::Store;
use crate::{activity, control, lock, running};

/// Existing on-disk save roots to watch, per eligible game.
/// Which game the daemon considers running, with an exit grace period: a
/// game only counts as exited after being gone for `EXIT_GRACE`. Proton
/// runs a short setup step under Steam's launcher, exits it, and starts the
/// real game seconds later (seen on SteamOS); without the grace the daemon
/// synced mid-launch.
///
/// Steam's own start/stop notifications (forwarded by the Decky plugin over
/// the control socket) take precedence: a hinted game counts as running
/// until its stop hint, which exits it immediately; afterwards the detector
/// seeing that game's processes wind down is ignored for a while.
pub(crate) struct GameSession {
    active: Option<u64>,
    gone_since: Option<std::time::Instant>,
    hinted: Option<u64>,
    stopped: Option<(u64, std::time::Instant)>,
}

impl GameSession {
    const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long after a stop hint the stopped game's lingering processes
    /// (Proton teardown, the launcher) are ignored.
    const STOP_QUIET: std::time::Duration = std::time::Duration::from_secs(30);

    fn new(running: Option<u64>) -> GameSession {
        GameSession { active: running, gone_since: None, hinted: None, stopped: None }
    }

    /// Steam says `app_id` started. Returns a different game that was
    /// running before, which is therefore done.
    fn hint_started(&mut self, app_id: u64) -> Option<u64> {
        let previous = self.active.filter(|a| *a != app_id);
        self.hinted = Some(app_id);
        self.active = Some(app_id);
        self.gone_since = None;
        self.stopped = None;
        previous
    }

    /// Steam says `app_id` stopped: it exits now, if it was the running game.
    fn hint_stopped(&mut self, app_id: u64, now: std::time::Instant) -> Option<u64> {
        self.stopped = Some((app_id, now));
        if self.hinted == Some(app_id) {
            self.hinted = None;
        }
        if self.active != Some(app_id) {
            return None;
        }
        self.active = None;
        self.gone_since = None;
        Some(app_id)
    }

    /// Feeds one detector poll (`seen`); returns the game that just exited.
    fn update(&mut self, seen: Option<u64>, now: std::time::Instant) -> Option<u64> {
        if let Some(hinted) = self.hinted {
            self.active = Some(hinted);
            return None;
        }
        let quiet = self.stopped.is_some_and(|(id, at)| seen == Some(id) && now.duration_since(at) < Self::STOP_QUIET);
        let seen = if quiet { None } else { seen };
        let exited = match self.active {
            Some(game) if seen == Some(game) => {
                self.gone_since = None;
                None
            }
            // Another game took over: the previous one is done for sure.
            Some(game) if seen.is_some() => Some(game),
            Some(game) => {
                let since = *self.gone_since.get_or_insert(now);
                (now.duration_since(since) >= Self::EXIT_GRACE).then_some(game)
            }
            None => None,
        };
        if exited.is_some() || self.active.is_none() {
            self.active = seen;
            self.gone_since = None;
        }
        exited
    }
}

/// The daemon's recent sync events, numbered, for `watch send events
/// <since>` (the Decky plugin polls it for toasts). Keeps the last 100.
#[derive(Default)]
pub(crate) struct EventLog {
    seq: u64,
    events: std::collections::VecDeque<(u64, u64, SyncEvent)>,
}

impl EventLog {
    const CAPACITY: usize = 100;

    fn record(&mut self, event: SyncEvent) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.seq += 1;
        self.events.push_back((self.seq, at, event));
        while self.events.len() > Self::CAPACITY {
            self.events.pop_front();
        }
    }

    /// `{"seq": latest, "events": [...]}` with the events after `since`;
    /// without `since`, just the latest number (a client's starting point).
    fn since(&self, since: Option<u64>) -> serde_json::Value {
        let since = since.unwrap_or(self.seq);
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|(seq, ..)| *seq > since)
            .map(|(seq, at, e)| {
                serde_json::json!({
                    "seq": seq, "at": at, "app_id": e.app_id, "game": e.game, "kind": e.kind, "detail": e.detail,
                })
            })
            .collect();
        serde_json::json!({"seq": self.seq, "events": events})
    }
}

/// What the daemon is doing, shared with the control socket so `status`
/// and `events` are answered at once — even while a sync holds up the
/// watch loop (the Decky plugin shows "Syncing…" from it).
#[derive(Default)]
pub(crate) struct DaemonState {
    log: EventLog,
    paused: bool,
    running_game: Option<u64>,
    pending_sync: Vec<u64>,
    watching_games: usize,
    /// The sync in progress: `Some(Some(id))` one game, `Some(None)` all.
    syncing: Option<Option<u64>>,
}

pub(crate) type SharedState = std::sync::Arc<std::sync::Mutex<DaemonState>>;

pub(crate) fn lock_state(state: &SharedState) -> std::sync::MutexGuard<'_, DaemonState> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DaemonState {
    fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "paused": self.paused,
            "running_game": self.running_game,
            "pending_sync": self.pending_sync,
            "watching_games": self.watching_games,
            "syncing": self.syncing.is_some(),
            "syncing_game": self.syncing.flatten(),
            "sync_phase": if self.syncing.is_some() { Some(activity::get()) } else { None },
        })
    }
}

/// `run_sync` for the daemon: marked as in progress while it runs, then its
/// events (or its failure) are logged.
pub(crate) fn daemon_sync(state: &SharedState, ctx: &Ctx, only: Option<u64>) -> Result<()> {
    // Disconnected on purpose: nothing to sync, and nothing to report.
    if matches!(config::Config::load().provider, config::ProviderConfig::None) {
        return Ok(());
    }
    lock_state(state).syncing = Some(only);
    activity::set("checking");
    let result = run_sync(ctx, only, false);
    activity::set("");
    let mut state = lock_state(state);
    state.syncing = None;
    match result {
        Ok(events) => {
            events.into_iter().for_each(|e| state.log.record(e));
            Ok(())
        }
        Err(err) => {
            state.log.record(SyncEvent { app_id: only, game: None, kind: "error", detail: format!("{err:#}") });
            Err(err)
        }
    }
}

/// One save location the daemon watches.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WatchRoot {
    app_id: u64,
    /// An existing save directory, or a save file.
    root: PathBuf,
    is_dir: bool,
    /// The rule's resolved path pattern, so a new file in a file root's
    /// folder (`Game3.dat` for `Game*.dat`) counts too.
    pattern: String,
}

impl WatchRoot {
    /// What to register with the OS watcher: a directory recursively, plus
    /// its parent non-recursively to see the directory itself being
    /// removed/recreated; for a file only its folder, non-recursively. Never
    /// a whole parent tree — `~/.local/share` recursively would pull in every
    /// app's data and, through Proton prefixes' `dosdevices/z:` link, the
    /// entire filesystem.
    fn watches(&self) -> Vec<(PathBuf, notify::RecursiveMode)> {
        let parent = self.root.parent().map(Path::to_path_buf);
        let mut out = Vec::new();
        if self.is_dir {
            out.push((self.root.clone(), notify::RecursiveMode::Recursive));
        }
        if let Some(parent) = parent {
            out.push((parent, notify::RecursiveMode::NonRecursive));
        }
        out
    }

    /// Does a change at `path` concern this save location?
    fn covers(&self, path: &Path) -> bool {
        if self.is_dir {
            return path.starts_with(&self.root);
        }
        path == self.root || glob::Pattern::new(&self.pattern).is_ok_and(|p| p.matches_path(path))
    }
}

pub(crate) fn watch_roots(ctx: &Ctx, cfg: &config::Config) -> Vec<WatchRoot> {
    let mut roots = Vec::new();
    for game in &ctx.games {
        if cfg.game(game.app_id).mode == ModeOverride::Off {
            continue;
        }
        let Some(merged) = ctx.merged_game(game.app_id) else { continue };
        for (template, rule) in &merged.files {
            if !rule.is_save() {
                continue;
            }
            let Some(res) = resolve::resolve_rule(template, rule, ctx.os, game) else { continue };
            let Some(pattern) = res.path else { continue };
            for root in resolve::existing_matches(&pattern) {
                let entry = WatchRoot { app_id: game.app_id, is_dir: root.is_dir(), root, pattern: pattern.clone() };
                if !roots.contains(&entry) {
                    roots.push(entry);
                }
            }
        }
    }
    roots
}

/// Registers `root`'s OS watches not already registered (a path watched
/// recursively needs no extra non-recursive watch).
pub(crate) fn register_watches(
    watcher: &mut impl notify::Watcher,
    registered: &mut HashMap<PathBuf, notify::RecursiveMode>,
    root: &WatchRoot,
) {
    for (path, mode) in root.watches() {
        match registered.get(&path) {
            Some(notify::RecursiveMode::Recursive) => continue,
            Some(_) if mode == notify::RecursiveMode::NonRecursive => continue,
            _ => {}
        }
        match watcher.watch(&path, mode) {
            Ok(()) => {
                registered.insert(path, mode);
            }
            Err(err) => eprintln!("cannot watch {}: {err}", path.display()),
        }
    }
}

/// Control messages into the watch loop, from the tray menu or the control
/// socket (`control.rs`, e.g. the Decky plugin).
pub enum WatchCommand {
    /// Sync one game, or all.
    Sync(Option<u64>),
    TogglePause,
    SetPaused(bool),
    /// Steam reported a game start / stop (see `GameSession` hints). A
    /// start with a reply channel syncs the game first and replies when
    /// done — the Decky plugin keeps the game paused until then.
    GameStarted(u64, Option<std::sync::mpsc::Sender<String>>),
    GameStopped(u64),
    Quit,
}

/// Answers one control-socket request: `status` and `events` from the
/// shared state, everything else by forwarding it to the watch loop.
pub(crate) fn remote_command(
    tx: &std::sync::mpsc::Sender<WatchCommand>,
    state: &SharedState,
    request: control::Request,
) -> String {
    use control::Request;
    let command = match request {
        Request::Status => return lock_state(state).status_json().to_string(),
        Request::Events(since) => return lock_state(state).log.since(since).to_string(),
        // Waits for the pre-launch sync (see WatchCommand::GameStarted); the
        // loop checks commands at least every 2 s, but a sync in progress
        // holds it up.
        Request::GameStarted(id) => {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            if tx.send(WatchCommand::GameStarted(id, Some(reply_tx))).is_err() {
                return "error: watch loop has stopped".into();
            }
            return reply_rx
                .recv_timeout(std::time::Duration::from_secs(150))
                .unwrap_or_else(|_| "error: daemon is busy (syncing); try again".into());
        }
        Request::GameStopped(id) => WatchCommand::GameStopped(id),
        Request::Sync(target) => WatchCommand::Sync(target),
        Request::Pause => WatchCommand::SetPaused(true),
        Request::Resume => WatchCommand::SetPaused(false),
        Request::Quit => WatchCommand::Quit,
    };
    match tx.send(command) {
        Ok(()) => "ok".into(),
        Err(_) => "error: watch loop has stopped".into(),
    }
}

/// Daemon entry: headless by default, `--tray` adds a status icon with
/// Sync now / Pause / Quit (Windows + macOS; Linux tray pending).
/// `watch status [--json]` reports whether a daemon is running instead;
/// `watch stop` / `watch start` stop it or launch it detached (used by the
/// GUI around self-updates, D20, and to restore it afterwards).
pub(crate) fn watch_cmd(args: &[String]) -> Result<()> {
    match positionals(args).first().map(String::as_str) {
        Some("stop") => return watch_stop(args),
        Some("start") => return watch_start(args),
        Some("send") => {
            let request = positionals(args)[1..].join(" ");
            control::parse(&request).context(control::USAGE)?;
            let reply = control::send(&request)?;
            println!("{reply}");
            if reply.starts_with("error") {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }
    if positionals(args).first().map(String::as_str) == Some("status") {
        let running = lock::daemon_status()?;
        if flag(args, "--json") {
            println!(
                "{}",
                serde_json::json!({"running": running.is_some(), "pid": running.flatten()})
            );
        } else {
            match running {
                Some(Some(pid)) => println!("watch daemon: running (pid {pid})"),
                Some(None) => println!("watch daemon: running"),
                None => println!("watch daemon: not running"),
            }
        }
        return Ok(());
    }
    // Held until this function returns, i.e. for the daemon's whole life
    // (tray mode runs its event loop inside tray_main).
    let _lock = match lock::acquire_daemon() {
        Ok(lock) => lock,
        Err(err) if err.is::<lock::AlreadyRunning>() => {
            eprintln!("yasgm: {err} — not starting a second one");
            std::process::exit(3);
        }
        Err(err) => return Err(err),
    };
    let settle = std::time::Duration::from_secs(
        flag_value(args, "--settle").and_then(|s| s.parse().ok()).unwrap_or(10),
    );
    if flag(args, "--tray") {
        #[cfg(any(windows, target_os = "macos"))]
        return crate::tray::tray_main(settle);
        #[cfg(not(any(windows, target_os = "macos")))]
        eprintln!("tray is not supported on this OS yet; running headless watch");
    }
    let (control_tx, control_rx) = std::sync::mpsc::channel();
    watch_loop(settle, (control_tx, control_rx), None)
}

/// Stops the running daemon (if any) and waits for its lock to free: a
/// clean `quit` over the control socket first (it finishes a sync in
/// progress), then terminate. Terminating is safe too — every local state
/// write is atomic (fsutil) and cloud versions are written before their
/// index entry — just not as polite.
pub(crate) fn watch_stop(args: &[String]) -> Result<()> {
    let was_running = lock::daemon_status()?;
    let quit_cleanly = was_running.is_some()
        && control::send("quit").is_ok_and(|reply| reply == "ok")
        && lock::wait_until_stopped(std::time::Duration::from_secs(30))?;
    if let (Some(pid), false) = (was_running, quit_cleanly) {
        let pid = pid.context("daemon is running but its pid is unknown (watch.pid missing)")?;
        terminate(pid, false);
        if !lock::wait_until_stopped(std::time::Duration::from_secs(10))? {
            terminate(pid, true);
            anyhow::ensure!(
                lock::wait_until_stopped(std::time::Duration::from_secs(5))?,
                "watch daemon (pid {pid}) did not stop"
            );
        }
    }
    if flag(args, "--json") {
        println!("{}", serde_json::json!({"was_running": was_running.is_some()}));
    } else if was_running.is_some() {
        println!("watch daemon stopped");
    } else {
        println!("watch daemon was not running");
    }
    Ok(())
}

pub(crate) fn terminate(pid: u32, force: bool) {
    #[cfg(unix)]
    let result = std::process::Command::new("kill")
        .args([if force { "-KILL" } else { "-TERM" }, &pid.to_string()])
        .output();
    #[cfg(windows)]
    let result = {
        let _ = force; // no graceful signal for a windowless process on Windows
        std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output()
    };
    if let Err(err) = result {
        eprintln!("could not signal pid {pid}: {err}");
    }
}

/// Launches `watch` (with the tray where supported) as a detached process
/// and waits until it holds the daemon lock. No-op if one already runs.
pub(crate) fn watch_start(args: &[String]) -> Result<()> {
    let pid = match lock::daemon_status()? {
        Some(pid) => pid,
        None => {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command.arg("watch");
            #[cfg(any(windows, target_os = "macos"))]
            command.arg("--tray");
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                // Own process group: not torn down with whoever launched us.
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
            }
            command.spawn().context("launching watch daemon")?;
            lock::wait_until_running(std::time::Duration::from_secs(10))?
                .context("watch daemon did not start within 10 s")?
        }
    };
    if flag(args, "--json") {
        println!("{}", serde_json::json!({"running": true, "pid": pid}));
    } else {
        println!("watch daemon running{}", pid.map(|p| format!(" (pid {p})")).unwrap_or_default());
    }
    Ok(())
}

/// FS-watch save dirs, debounce, and sync when a game's saves settle and the
/// game isn't running (or as soon as it exits). The launch wrapper
/// (`yasgm run`) remains the strongest guarantee; this covers unwrapped
/// launches.
pub(crate) fn watch_loop(
    settle: std::time::Duration,
    (control_tx, control_rx): (std::sync::mpsc::Sender<WatchCommand>, std::sync::mpsc::Receiver<WatchCommand>),
    status: Option<std::sync::mpsc::Sender<String>>,
) -> Result<()> {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const RESCAN_EVERY: Duration = Duration::from_secs(300);
    /// How often to look for newer saves from other machines.
    const CLOUD_CHECK_EVERY: Duration = Duration::from_secs(600);
    /// How often to check whether games, settings or the manifest changed.
    const RELOAD_CHECK_EVERY: Duration = Duration::from_secs(30);
    /// Re-fetch the manifest (a conditional download) at least this often.
    const MANIFEST_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
    let report = |line: &str| {
        if let Some(tx) = &status {
            let _ = tx.send(line.to_owned());
        }
    };

    // Tests shorten the cloud check (`YASGM_CLOUD_CHECK_SECS`).
    let cloud_check_every = std::env::var("YASGM_CLOUD_CHECK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(CLOUD_CHECK_EVERY, Duration::from_secs);
    let mut ctx = load_ctx()?;
    let mut fingerprint = ctx.inputs_fingerprint();
    let mut loaded_at = Instant::now();

    let state = SharedState::default();
    let control_state = state.clone();
    if let Err(err) = control::serve(move |request| remote_command(&control_tx, &control_state, request)) {
        eprintln!("control socket unavailable (watch send won't reach this daemon): {err:#}");
    }

    println!("initial sync pass…");
    report("initial sync…");
    if let Err(err) = daemon_sync(&state, &ctx, None) {
        eprintln!("initial sync failed (will keep watching): {err:#}");
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })?;
    let mut roots = watch_roots(&ctx, &config::Config::load());
    let mut registered = HashMap::new();
    for root in &roots {
        register_watches(&mut watcher, &mut registered, root);
    }
    let n_games = roots.iter().map(|r| r.app_id).collect::<std::collections::HashSet<_>>().len();
    println!(
        "watching {} paths across {n_games} games (settle {}s); Ctrl-C to stop",
        roots.len(),
        settle.as_secs()
    );
    report(&format!("watching {n_games} games"));
    lock_state(&state).watching_games = n_games;

    let mut dirty: HashMap<u64, Instant> = HashMap::new();
    let mut detector = running::RunningDetector::new();
    let mut session = GameSession::new(detector.poll(&ctx.root, &ctx.games));
    let mut last_rescan = Instant::now();
    let mut last_reload_check = Instant::now();
    let mut last_cloud_check = Instant::now();
    let mut paused = false;
    // The loop's view, for `status` over the control socket.
    let publish = |dirty: &HashMap<u64, Instant>, paused: bool, running_game: Option<u64>| {
        let mut pending: Vec<u64> = dirty.keys().copied().collect();
        pending.sort();
        let mut shared = lock_state(&state);
        shared.paused = paused;
        shared.running_game = running_game;
        shared.pending_sync = pending;
    };

    let handle_event = |event: notify::Result<notify::Event>,
                            dirty: &mut HashMap<u64, Instant>,
                            roots: &[WatchRoot]| {
        let Ok(event) = event else { return };
        if event.kind.is_access() {
            return; // reads (e.g. our own snapshot) aren't changes
        }
        for path in &event.paths {
            for root in roots.iter().filter(|r| r.covers(path)) {
                if dirty.insert(root.app_id, Instant::now()).is_none() {
                    println!("change detected for app {} ({})", root.app_id, path.display());
                }
            }
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(event) => handle_event(event, &mut dirty, &roots),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(event) = rx.try_recv() {
            handle_event(event, &mut dirty, &roots);
        }

        let mut exits: Vec<u64> = Vec::new();
        while let Ok(command) = control_rx.try_recv() {
            match command {
                WatchCommand::Quit => {
                    report("stopped");
                    control::remove_socket();
                    return Ok(());
                }
                WatchCommand::TogglePause | WatchCommand::SetPaused(_) => {
                    paused = match command {
                        WatchCommand::SetPaused(value) => value,
                        _ => !paused,
                    };
                    println!("{}", if paused { "paused" } else { "resumed" });
                    report(if paused { "paused" } else { "resumed — watching" });
                }
                WatchCommand::Sync(target) => {
                    report("syncing…");
                    match daemon_sync(&state, &ctx, target) {
                        Ok(()) => report("synced (manual)"),
                        Err(err) => {
                            eprintln!("manual sync failed: {err:#}");
                            report("sync failed — see log");
                        }
                    }
                    match target {
                        Some(app_id) => {
                            dirty.remove(&app_id);
                        }
                        None => dirty.clear(),
                    }
                }
                // Steam reports non-Steam shortcuts and tools too; only
                // installed Steam games matter.
                WatchCommand::GameStarted(app_id, reply) => {
                    if ctx.games.iter().any(|g| g.app_id == app_id) {
                        // The plugin holds the game paused until we reply:
                        // pull newer cloud saves before it reads them.
                        if !paused {
                            println!("Steam reports app {app_id} starting — syncing it first");
                            if let Err(err) = daemon_sync(&state, &ctx, Some(app_id)) {
                                eprintln!("pre-launch sync of {app_id} failed: {err:#}");
                            }
                            dirty.remove(&app_id);
                        } else {
                            println!("Steam reports app {app_id} started");
                        }
                        exits.extend(session.hint_started(app_id));
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send("ok".into());
                    }
                }
                WatchCommand::GameStopped(app_id) if !ctx.games.iter().any(|g| g.app_id == app_id) => {}
                WatchCommand::GameStopped(app_id) => {
                    println!("Steam reports app {app_id} stopped");
                    exits.extend(session.hint_stopped(app_id, Instant::now()));
                }
            }
        }

        exits.extend(session.update(detector.poll(&ctx.root, &ctx.games), Instant::now()));
        // Still "running" during the exit grace period, so settled saves wait too.
        let running_now = session.active;
        publish(&dirty, paused, running_now);
        for prev in exits {
            if !paused {
                println!("app {prev} exited — syncing");
                report("game exited — syncing…");
                match daemon_sync(&state, &ctx, Some(prev)) {
                    Ok(()) => report("synced after game exit"),
                    Err(err) => {
                        eprintln!("sync after exit of {prev} failed: {err:#}");
                        report("sync failed — see log");
                    }
                }
                dirty.remove(&prev);
            }
        }

        if !paused {
            let settled: Vec<u64> = dirty
                .iter()
                .filter(|(app_id, since)| {
                    since.elapsed() >= settle && running_now != Some(**app_id)
                })
                .map(|(app_id, _)| *app_id)
                .collect();
            for app_id in settled {
                println!("saves settled for app {app_id} — syncing");
                report("saves changed — syncing…");
                match daemon_sync(&state, &ctx, Some(app_id)) {
                    Ok(()) => report("synced"),
                    Err(err) => {
                        eprintln!("sync of {app_id} failed: {err:#}");
                        report("sync failed — see log");
                    }
                }
                dirty.remove(&app_id);
            }
        }
        publish(&dirty, paused, running_now);

        // Newer saves from other machines (otherwise only seen at startup
        // and, on the Deck, before a game launches).
        if !paused && last_cloud_check.elapsed() >= cloud_check_every {
            last_cloud_check = Instant::now();
            match cloud_changed_games(&ctx, running_now) {
                Ok(ids) => {
                    for app_id in ids {
                        println!("newer saves in the cloud for app {app_id} — syncing");
                        report("newer saves in the cloud — syncing…");
                        if let Err(err) = daemon_sync(&state, &ctx, Some(app_id)) {
                            eprintln!("sync of {app_id} failed: {err:#}");
                        }
                        dirty.remove(&app_id);
                    }
                    publish(&dirty, paused, running_now);
                }
                Err(err) => eprintln!("cloud check skipped: {err:#}"),
            }
        }

        // Games installed or removed, settings changed (a mode set to off),
        // another Steam account, a newer manifest: reload what we work from.
        let mut reloaded = false;
        if last_reload_check.elapsed() >= RELOAD_CHECK_EVERY {
            last_reload_check = Instant::now();
            let now = ctx.inputs_fingerprint();
            if now != fingerprint || loaded_at.elapsed() >= MANIFEST_MAX_AGE {
                match load_ctx() {
                    Ok(fresh) => {
                        println!("games, settings or manifest changed — reloaded ({} games)", fresh.games.len());
                        ctx = fresh;
                        reloaded = true;
                    }
                    Err(err) => eprintln!("reload failed, keeping the previous game list: {err:#}"),
                }
                fingerprint = ctx.inputs_fingerprint();
                loaded_at = Instant::now();
            }
        }

        // Pick up save dirs that didn't exist at startup (first-ever save).
        if reloaded || last_rescan.elapsed() >= RESCAN_EVERY {
            last_rescan = Instant::now();
            let fresh = watch_roots(&ctx, &config::Config::load());
            for root in fresh.iter().filter(|r| !roots.contains(r)) {
                register_watches(&mut watcher, &mut registered, root);
                println!("now watching {} (app {})", root.root.display(), root.app_id);
            }
            roots = fresh;
            let n_games = roots.iter().map(|r| r.app_id).collect::<std::collections::HashSet<_>>().len();
            lock_state(&state).watching_games = n_games;
        }
    }
    Ok(())
}

/// Two-way games (not running now) whose cloud head isn't what this machine
/// last synced: another machine uploaded newer saves. One index request
/// per game; games that can't be checked (offline) are skipped.
pub(crate) fn cloud_changed_games(ctx: &Ctx, running: Option<u64>) -> Result<Vec<u64>> {
    let cfg = config::Config::load();
    if matches!(cfg.provider, config::ProviderConfig::None) {
        return Ok(Vec::new());
    }
    let state = config::State::load();
    // Backup-only games never download automatically; they're reported by
    // the normal syncs, not every few minutes.
    let ids: Vec<u64> = ctx
        .games
        .iter()
        .map(|g| g.app_id)
        .filter(|&id| Some(id) != running && ctx.backup_only(&cfg, id) == Some(false))
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let store = ctx.cloud_store()?;
    Ok(load_indexes(&store, &ids)
        .into_iter()
        .filter_map(|(id, index)| {
            let index = index.ok()??;
            let head = Store::head(&index).filter(|h| !h.pending)?;
            let last = state.get(&ctx.account, id).map(|s| s.last_hash.as_str());
            (last != Some(head.content_hash.as_str())).then_some(id)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{DaemonState, EventLog, GameSession, WatchRoot};
    use crate::commands::sync::SyncEvent;
    use std::path::{Path, PathBuf};

    #[test]
    fn proton_setup_gap_is_not_an_exit() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut session = GameSession::new(None);
        // Proton setup step runs, exits, real game starts 3 s later.
        assert_eq!(session.update(Some(2140020), at(0)), None);
        assert_eq!(session.update(None, at(1)), None);
        assert_eq!(session.active, Some(2140020), "still running during the grace period");
        assert_eq!(session.update(None, at(3)), None);
        assert_eq!(session.update(Some(2140020), at(4)), None);
        // Real exit: reported once, after the grace period.
        assert_eq!(session.update(None, at(60)), None);
        assert_eq!(session.update(None, at(65)), None);
        assert_eq!(session.update(None, at(70)), Some(2140020));
        assert_eq!(session.active, None);
        assert_eq!(session.update(None, at(80)), None, "no second exit");
    }
    #[test]
    fn event_log_returns_only_newer_events_and_caps_its_size() {
        let event = |kind| SyncEvent { app_id: Some(1), game: Some("G".into()), kind, detail: String::new() };
        let mut log = EventLog::default();
        assert_eq!(log.since(None), serde_json::json!({"seq": 0, "events": []}));
        log.record(event("uploaded"));
        log.record(event("conflict"));
        let start = log.since(None);
        assert_eq!(start["seq"], 2);
        assert!(start["events"].as_array().unwrap().is_empty(), "no backlog for a new client");
        let newer = log.since(Some(1));
        assert_eq!(newer["events"].as_array().unwrap().len(), 1);
        assert_eq!(newer["events"][0]["kind"], "conflict");
        for _ in 0..150 {
            log.record(event("uploaded"));
        }
        assert_eq!(log.events.len(), EventLog::CAPACITY);
        assert_eq!(log.since(Some(0))["seq"], 152);
    }
    #[test]
    fn status_reports_the_sync_in_progress() {
        let mut state = DaemonState { watching_games: 3, ..Default::default() };
        assert_eq!(state.status_json()["syncing"], false);
        assert_eq!(state.status_json()["syncing_game"], serde_json::Value::Null);
        state.syncing = Some(Some(42));
        assert_eq!(state.status_json()["syncing"], true);
        assert_eq!(state.status_json()["syncing_game"], 42);
        state.syncing = Some(None);
        assert_eq!(state.status_json()["syncing"], true, "a sync of every game");
        assert_eq!(state.status_json()["syncing_game"], serde_json::Value::Null);
        assert_eq!(state.status_json()["watching_games"], 3);
    }
    #[test]
    fn steam_hints_override_detection() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut session = GameSession::new(None);
        assert_eq!(session.hint_started(42), None);
        // Detection missing the game doesn't end a hinted session.
        assert_eq!(session.update(None, at(30)), None);
        assert_eq!(session.active, Some(42));
        // Stop hint exits at once, no grace period.
        assert_eq!(session.hint_stopped(42, at(31)), Some(42));
        assert_eq!(session.active, None);
        // The launcher lingering while Proton shuts down isn't a new session.
        assert_eq!(session.update(Some(42), at(33)), None);
        assert_eq!(session.active, None);
        assert_eq!(session.update(None, at(50)), None, "no second exit");
        // A stop hint for a game that isn't running does nothing.
        assert_eq!(session.hint_stopped(7, at(60)), None);
    }
    #[test]
    fn switching_games_exits_the_previous_one_immediately() {
        use std::time::Instant;
        let now = Instant::now();
        let mut session = GameSession::new(Some(1));
        assert_eq!(session.update(Some(2), now), Some(1));
        assert_eq!(session.active, Some(2));
    }
    #[test]
    fn watch_root_ignores_siblings_of_a_save_directory() {
        let root = WatchRoot {
            app_id: 736260,
            root: PathBuf::from("/home/u/.local/share/Baba_Is_You"),
            is_dir: true,
            pattern: "/home/u/.local/share/Baba_Is_You".into(),
        };
        assert!(root.covers(Path::new("/home/u/.local/share/Baba_Is_You/ba.ba")));
        assert!(root.covers(Path::new("/home/u/.local/share/Baba_Is_You")));
        assert!(!root.covers(Path::new("/home/u/.local/share/sddm/state.conf")));
        assert!(!root.covers(Path::new("/home/u/.local/share/Baba_Is_You_old/x")));
        // Never the parent tree recursively.
        let watches = root.watches();
        assert!(watches.contains(&(PathBuf::from("/home/u/.local/share/Baba_Is_You"), notify::RecursiveMode::Recursive)));
        assert!(watches.contains(&(PathBuf::from("/home/u/.local/share"), notify::RecursiveMode::NonRecursive)));
        assert!(!watches.contains(&(PathBuf::from("/home/u/.local/share"), notify::RecursiveMode::Recursive)));
    }
    #[test]
    fn watch_root_for_a_file_pattern_catches_new_matching_files_only() {
        let root = WatchRoot {
            app_id: 331920,
            root: PathBuf::from("/games/ATD/Savegame/Game1.dat"),
            is_dir: false,
            pattern: "/games/ATD/Savegame/Game*.dat".into(),
        };
        assert!(root.covers(Path::new("/games/ATD/Savegame/Game1.dat")));
        assert!(root.covers(Path::new("/games/ATD/Savegame/Game7.dat")), "a new save matching the pattern");
        assert!(!root.covers(Path::new("/games/ATD/Savegame/Options.cfg")));
        assert_eq!(root.watches(), vec![(PathBuf::from("/games/ATD/Savegame"), notify::RecursiveMode::NonRecursive)]);
    }
}
