//! Local control channel for the watch daemon: a Unix socket next to the
//! daemon lock (`<data dir>/yasgm/watch.sock`, owner-only). One request per
//! connection — a line of text — answered with one line: `ok`, JSON for
//! `status`, or `error: …`. `yasgm watch send …` is the client; the Decky
//! plugin forwards Steam's game start/stop notifications through it, and
//! `watch stop` uses `quit` for a clean shutdown before resorting to
//! signals. Not available on Windows (no Decky there; the GUI stops the
//! daemon with `watch stop`'s terminate fallback).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Steam says this game started; it counts as running until
    /// `GameStopped`, whatever process detection sees.
    GameStarted(u64),
    /// Steam says this game stopped: sync it now (no grace period).
    GameStopped(u64),
    /// Sync one game, or every game.
    Sync(Option<u64>),
    Pause,
    Resume,
    Quit,
    Status,
    /// Sync events after this number (none: just the latest number).
    Events(Option<u64>),
}

pub const USAGE: &str = "usage: yasgm watch send game-started <appid> | game-stopped <appid> | sync [appid] | \
     pause | resume | quit | status | events [since]";

pub fn parse(line: &str) -> Result<Request> {
    let mut words = line.split_whitespace();
    let command = words.next().unwrap_or_default();
    let app_id = |word: Option<&str>| -> Result<u64> {
        word.context("missing appid")?.parse().context("appid must be a number")
    };
    let request = match command {
        "game-started" => Request::GameStarted(app_id(words.next())?),
        "game-stopped" => Request::GameStopped(app_id(words.next())?),
        "sync" => Request::Sync(words.next().map(|w| app_id(Some(w))).transpose()?),
        "pause" => Request::Pause,
        "resume" => Request::Resume,
        "quit" => Request::Quit,
        "status" => Request::Status,
        "events" => Request::Events(words.next().map(|w| w.parse().context("since must be a number")).transpose()?),
        "" => bail!("empty request"),
        other => bail!("unknown request {other:?}"),
    };
    if let Some(extra) = words.next() {
        bail!("unexpected {extra:?} after {command}");
    }
    Ok(request)
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(crate::lock::daemon_dir()?.join("watch.sock"))
}

/// Starts answering requests on the socket in a background thread. Only
/// call while holding the daemon lock: a leftover socket file from a
/// crashed daemon is removed first.
#[cfg(unix)]
pub fn serve(handler: impl Fn(Request) -> String + Send + Sync + 'static) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    let path = socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let handler = std::sync::Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // One thread per request: `game-started` waits for a sync, and
            // `status` polls must still be answered meanwhile.
            let handler = handler.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    return;
                }
                let reply = match parse(&line) {
                    Ok(request) => handler(request),
                    Err(err) => format!("error: {err}"),
                };
                let _ = writeln!(stream, "{reply}");
            });
        }
    });
    Ok(())
}

/// Sends one request to the running daemon and returns its reply line.
#[cfg(unix)]
pub fn send(line: &str) -> Result<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("no watch daemon listening on {}", path.display()))?;
    // `game-started` waits for the daemon's pre-launch sync.
    stream.set_read_timeout(Some(std::time::Duration::from_secs(180)))?;
    writeln!(stream, "{}", line.trim())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).context("reading the daemon's reply")?;
    Ok(reply.trim_end().to_owned())
}

#[cfg(not(unix))]
pub fn serve(_handler: impl Fn(Request) -> String + Send + Sync + 'static) -> Result<()> {
    Ok(()) // no control socket on Windows; see module docs
}

#[cfg(not(unix))]
pub fn send(_line: &str) -> Result<String> {
    bail!("the watch control channel isn't available on Windows")
}

pub fn remove_socket() {
    if let Ok(path) = socket_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        assert_eq!(parse("game-started 2140020\n").unwrap(), Request::GameStarted(2140020));
        assert_eq!(parse("game-stopped 736260").unwrap(), Request::GameStopped(736260));
        assert_eq!(parse("sync").unwrap(), Request::Sync(None));
        assert_eq!(parse("sync 42").unwrap(), Request::Sync(Some(42)));
        assert_eq!(parse("  status ").unwrap(), Request::Status);
        assert_eq!(parse("events").unwrap(), Request::Events(None));
        assert_eq!(parse("events 12").unwrap(), Request::Events(Some(12)));
        assert!(parse("events x").is_err());
        assert!(parse("game-started").is_err());
        assert!(parse("game-started abc").is_err());
        assert!(parse("quit now").is_err());
        assert!(parse("reboot").is_err());
        assert!(parse("").is_err());
    }
}
