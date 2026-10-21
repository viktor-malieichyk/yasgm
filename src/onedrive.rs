//! OneDrive cloud provider — Microsoft Graph.
//! Auth: OAuth2 public client with PKCE + localhost loopback redirect, and a
//! device-code fallback for browserless environments (Steam Deck Gaming
//! Mode). Tokens are cached in the user config dir with 0600 permissions;
//! OS keychain integration is a later hardening task.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

pub const CLIENT_ID: &str = "a79772b2-0da9-4af5-bc70-0aed51abab0b";
// The /consumers tenant targets personal Microsoft accounts — the only kind
// that supports Files.ReadWrite.AppFolder. (/common gives server_error when
// an MSA requests that scope.)
const AUTHORIZE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const SCOPES: &str = "Files.ReadWrite.AppFolder offline_access";
const GRAPH: &str = "https://graph.microsoft.com/v1.0";

#[derive(Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

/// One agent for every Microsoft request, so connections (and their TLS
/// handshakes) are reused across calls — a fresh `ureq::get` per request
/// made each Graph call pay a full handshake.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(15))
            .timeout_read(std::time::Duration::from_secs(120))
            .build()
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn token_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no config directory on this platform")?
        .join("yasgm");
    fs::create_dir_all(&dir)?;
    Ok(dir.join("tokens.json"))
}

/// Removes the cached sign-in from this device (disconnect). The access
/// YASGM was granted on the Microsoft account itself stays until revoked at
/// https://account.live.com/consent/Manage.
pub fn delete_tokens() -> Result<()> {
    match fs::remove_file(token_path()?) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("removing the cached OneDrive sign-in"),
    }
}

pub fn save_tokens(tokens: &Tokens) -> Result<()> {
    let path = token_path()?;
    // 0600 is applied to the temp file before the rename, so the token file
    // is never briefly readable by others.
    crate::fsutil::write_atomic_with(&path, &serde_json::to_vec_pretty(tokens)?, |_tmp| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(_tmp, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })?;
    Ok(())
}

pub fn load_tokens() -> Option<Tokens> {
    let path = token_path().ok()?;
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

/// Inner `Err` carries the OAuth error code (e.g. `authorization_pending`),
/// which callers may treat as non-fatal.
fn token_request(params: &[(&str, &str)]) -> Result<std::result::Result<TokenResponse, String>> {
    let body = match agent().post(TOKEN_URL).send_form(params) {
        Ok(resp) => resp.into_string()?,
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let err: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let code = err
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error")
                .to_owned();
            return Ok(Err(code));
        }
        Err(err) => return Err(crate::provider::Offline(format!("Microsoft sign-in service: {err}")).into()),
    };
    Ok(Ok(serde_json::from_str(&body).context("parsing token response")?))
}

fn into_tokens(resp: TokenResponse, previous_refresh: Option<String>) -> Result<Tokens> {
    let refresh_token = resp
        .refresh_token
        .or(previous_refresh)
        .context("no refresh token granted (offline_access scope missing?)")?;
    Ok(Tokens {
        access_token: resp.access_token,
        refresh_token,
        expires_at: unix_now() + resp.expires_in.saturating_sub(60),
    })
}

fn random_urlsafe() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "windows") {
        // `cmd /C start <url>` routes the URL through cmd.exe's own command
        // parser, which treats `&` (ubiquitous in OAuth query strings) as a
        // command separator and mangles quoting around the target/title
        // arguments. Invoking explorer.exe directly hands the URL to
        // CreateProcess as a single argument with no shell parsing at all.
        std::process::Command::new("explorer").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if result.is_err() {
        eprintln!("(could not launch a browser automatically)");
    }
}

/// PKCE + localhost loopback sign-in (default on desktop).
pub fn login_interactive() -> Result<Tokens> {
    let verifier = random_urlsafe();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe();

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/");

    let auth_url = Url::parse_with_params(
        AUTHORIZE_URL,
        [
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri.as_str()),
            ("scope", SCOPES),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("prompt", "select_account"),
        ],
    )?;

    eprintln!("Opening your browser for Microsoft sign-in…");
    eprintln!("If nothing opens, visit this URL manually:\n\n{auth_url}\n");
    open_browser(auth_url.as_str());

    let code = wait_for_code(&listener, &state)?;
    match token_request(&[
        ("client_id", CLIENT_ID),
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", verifier.as_str()),
        ("scope", SCOPES),
    ])? {
        Ok(resp) => into_tokens(resp, None),
        Err(code) => bail!("sign-in failed: {code}"),
    }
}

fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        let request_line = {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            line
        };
        let path = request_line.split_whitespace().nth(1).unwrap_or("/");
        let url = Url::parse(&format!("http://localhost{path}"))?;

        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error_description" => error = Some(value.into_owned()),
                "error" if error.is_none() => error = Some(value.into_owned()),
                _ => {}
            }
        }

        if code.is_none() && error.is_none() {
            // Stray request (e.g. favicon); keep waiting.
            respond(&mut stream, "404 Not Found", "");
            continue;
        }
        if let Some(err) = error {
            respond(&mut stream, "200 OK", "YASGM: sign-in failed. You can close this tab.");
            bail!("authorization error: {err}");
        }
        if state.as_deref() != Some(expected_state) {
            respond(&mut stream, "400 Bad Request", "YASGM: unexpected response.");
            bail!("authorization response state mismatch");
        }
        respond(&mut stream, "200 OK", "YASGM: sign-in complete. You can close this tab.");
        return Ok(code.expect("checked above"));
    }
    bail!("local listener closed before receiving the sign-in response")
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let html =
        format!("<html><body style=\"font-family:sans-serif\"><h2>{body}</h2></body></html>");
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        )
        .as_bytes(),
    );
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    message: Option<String>,
    #[serde(default)]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}

/// How often to remind the user we're still waiting, so a long silent wait
/// (e.g. switching to a phone to sign in) doesn't look hung.
const PING_EVERY_SECS: u64 = 30;

/// Progress of a device-code sign-in, for whoever shows it to the user.
pub enum DeviceEvent<'a> {
    /// Show this: open `verification_uri` on any device, enter `user_code`.
    Code { user_code: &'a str, verification_uri: &'a str, expires_in: u64, message: Option<&'a str> },
    /// Still waiting for the user (sent every 30 s).
    Waiting { elapsed: u64 },
}

/// Device-code sign-in (Steam Deck Gaming Mode, SSH sessions, …).
pub fn login_device(on_event: &dyn Fn(DeviceEvent)) -> Result<Tokens> {
    let resp = agent().post(DEVICE_CODE_URL)
        .send_form(&[("client_id", CLIENT_ID), ("scope", SCOPES)])
        .context("requesting device code")?;
    let dc: DeviceCodeResponse = serde_json::from_str(&resp.into_string()?)?;

    let mut interval = dc.interval.max(1);
    // Entra's default device-code lifetime is 900s; fall back to that if the
    // server omits expires_in, so a local guard still exists as a backstop
    // alongside the server's own expired_token error.
    let deadline = if dc.expires_in > 0 { dc.expires_in } else { 900 };
    on_event(DeviceEvent::Code {
        user_code: &dc.user_code,
        verification_uri: &dc.verification_uri,
        expires_in: deadline,
        message: dc.message.as_deref(),
    });
    let mut waited = 0u64;
    let mut last_ping = 0u64;
    loop {
        sleep(Duration::from_secs(interval));
        waited += interval;
        if waited - last_ping >= PING_EVERY_SECS {
            on_event(DeviceEvent::Waiting { elapsed: waited });
            last_ping = waited;
        }
        match token_request(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", dc.device_code.as_str()),
            ("scope", SCOPES),
        ])? {
            Ok(resp) => return into_tokens(resp, None),
            Err(code) => match code.as_str() {
                "authorization_pending" if waited < deadline => continue,
                "authorization_pending" | "expired_token" => bail!(
                    "the code expired before sign-in completed — run `yasgm auth --device` again"
                ),
                "slow_down" => interval += 5,
                "authorization_declined" => bail!("sign-in was declined"),
                other => bail!("device sign-in failed: {other}"),
            },
        }
    }
}

pub fn refresh(tokens: Tokens) -> Result<Tokens> {
    match token_request(&[
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", tokens.refresh_token.as_str()),
        ("scope", SCOPES),
    ])? {
        Ok(resp) => {
            let tokens = into_tokens(resp, Some(tokens.refresh_token))?;
            save_tokens(&tokens)?;
            Ok(tokens)
        }
        Err(code) => bail!("token refresh failed ({code}); run `yasgm auth` again"),
    }
}

/// Valid access token from the cache, refreshing if expired.
fn approot_url(rel: &str, suffix: &str) -> String {
    format!("{GRAPH}/me/drive/special/approot:/{rel}{suffix}")
}

const SIMPLE_UPLOAD_LIMIT: usize = 4_000_000;
// Graph requires chunk sizes in multiples of 320 KiB; 10 MiB qualifies.
const CHUNK: usize = 10_485_760;
/// Tries per request when Graph is throttling (429) or failing (5xx).
const ATTEMPTS: u32 = 4;

/// The AppFolder scope can't read `/me/drive` for proactive quota checks (see
/// DESIGN.md's Azure appendix), so quota exhaustion is only detectable from a
/// failed upload: Graph returns 507 Insufficient Storage. This marker type
/// lets callers distinguish "OneDrive is full" from other upload failures
/// (via `anyhow::Error::downcast_ref`) to stop a batch cleanly instead of
/// dumping a raw Graph error per game.
#[derive(Debug)]
pub struct QuotaExceeded;

impl std::fmt::Display for QuotaExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OneDrive is out of storage space")
    }
}

impl std::error::Error for QuotaExceeded {}

/// How Graph answered a request that didn't fail outright.
enum Reply {
    Ok(Box<ureq::Response>),
    NotFound,
    /// 412/409: a conditional request found the item changed (or present).
    Changed,
}

/// Graph access for one run. The access token is fetched on first use —
/// so a run that's offline with an expired token still gets as far as the
/// pending store (D18) instead of failing up front — and renewed once if
/// Graph rejects it (401: expired mid-run, revoked). Throttling (429) and
/// server errors (5xx) are retried, honouring `Retry-After`; a request that
/// can't reach Microsoft at all is reported as `provider::Offline`.
pub struct Session {
    token: std::sync::Mutex<Option<String>>,
}

impl Default for Session {
    fn default() -> Self {
        Session::new()
    }
}

// ureq 2's error type is large (it carries the response); every request
// closure here returns it. That's ureq's API, not something to box.
#[allow(clippy::result_large_err)]
impl Session {
    pub fn new() -> Session {
        Session { token: std::sync::Mutex::new(None) }
    }

    fn token(&self, renew: bool) -> Result<String> {
        let mut cached = self.token.lock().unwrap_or_else(|p| p.into_inner());
        if renew || cached.is_none() {
            let tokens = load_tokens().context("not signed in — run `yasgm auth` first")?;
            let tokens = if renew || tokens.expires_at <= unix_now() { refresh(tokens)? } else { tokens };
            *cached = Some(tokens.access_token);
        }
        Ok(cached.clone().expect("set above"))
    }

    /// Sends the request `make` builds (given the bearer header, for Graph
    /// calls; `authed: false` for pre-authorized URLs) with the retries above.
    fn send(&self, what: &str, authed: bool, make: impl Fn(Option<&str>) -> Result<ureq::Response, ureq::Error>) -> Result<Reply> {
        let mut renewed = false;
        for attempt in 1..=ATTEMPTS {
            let bearer = if authed { Some(format!("Bearer {}", self.token(false)?)) } else { None };
            match make(bearer.as_deref()) {
                Ok(resp) => return Ok(Reply::Ok(Box::new(resp))),
                Err(ureq::Error::Status(404, _)) => return Ok(Reply::NotFound),
                Err(ureq::Error::Status(409 | 412, _)) => return Ok(Reply::Changed),
                Err(ureq::Error::Status(507, _)) => return Err(QuotaExceeded.into()),
                Err(ureq::Error::Status(401, _)) if authed && !renewed => {
                    renewed = true;
                    self.token(true)?;
                }
                Err(ureq::Error::Status(code @ (429 | 500 | 502 | 503 | 504), resp)) if attempt < ATTEMPTS => {
                    let wait = resp.header("Retry-After").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1 << attempt);
                    eprintln!("OneDrive answered {code} to {what}; retrying in {}s", wait.min(60));
                    sleep(Duration::from_secs(wait.min(60)));
                }
                Err(ureq::Error::Status(code, resp)) => {
                    bail!("{what} failed ({code}): {}", resp.into_string().unwrap_or_default())
                }
                Err(ureq::Error::Transport(err)) if attempt < 2 => {
                    let _ = err;
                    sleep(Duration::from_secs(2));
                }
                Err(ureq::Error::Transport(err)) => {
                    return Err(crate::provider::Offline(format!("{what}: {err}")).into());
                }
            }
        }
        bail!("{what}: OneDrive kept refusing (throttled or failing); try again later")
    }

    fn ok(&self, what: &str, authed: bool, make: impl Fn(Option<&str>) -> Result<ureq::Response, ureq::Error>) -> Result<Option<ureq::Response>> {
        match self.send(what, authed, make)? {
            Reply::Ok(resp) => Ok(Some(*resp)),
            Reply::NotFound => Ok(None),
            Reply::Changed => bail!("{what}: unexpected conflict"),
        }
    }

    pub fn graph_get(&self, path: &str) -> Result<serde_json::Value> {
        let what = format!("Graph {path}");
        let resp = self
            .ok(&what, true, |auth| agent().get(&format!("{GRAPH}{path}")).set("Authorization", auth.unwrap_or_default()).call())?
            .with_context(|| format!("{what}: not found"))?;
        Ok(serde_json::from_str(&resp.into_string()?)?)
    }

    pub fn item_get(&self, rel: &str) -> Result<Option<serde_json::Value>> {
        match self.ok(&format!("get {rel}"), true, |auth| agent().get(&approot_url(rel, "")).set("Authorization", auth.unwrap_or_default()).call())? {
            Some(resp) => Ok(Some(serde_json::from_str(&resp.into_string()?)?)),
            None => Ok(None),
        }
    }

    pub fn list_children(&self, rel: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut url = approot_url(rel, ":/children?$select=name&$top=999");
        loop {
            let Some(resp) = self.ok(&format!("list {rel}"), true, |auth| agent().get(&url).set("Authorization", auth.unwrap_or_default()).call())? else {
                return Ok(names);
            };
            let page: serde_json::Value = serde_json::from_str(&resp.into_string()?)?;
            if let Some(items) = page["value"].as_array() {
                names.extend(items.iter().filter_map(|i| i["name"].as_str().map(str::to_owned)));
            }
            match page["@odata.nextLink"].as_str() {
                Some(next) => url = next.to_owned(),
                None => return Ok(names),
            }
        }
    }

    /// The file's bytes and eTag, or None if it doesn't exist.
    pub fn download_tagged(&self, rel: &str) -> Result<Option<(Vec<u8>, String)>> {
        let Some(item) = self.item_get(rel)? else {
            return Ok(None);
        };
        // The pre-authorized download URL avoids auth-header issues on the
        // CDN redirect target.
        let url = item["@microsoft.graph.downloadUrl"].as_str().with_context(|| format!("no download URL for {rel}"))?;
        let resp = self
            .ok(&format!("download {rel}"), false, |_| agent().get(url).call())?
            .with_context(|| format!("{rel} vanished while downloading"))?;
        let mut bytes = Vec::new();
        resp.into_reader().read_to_end(&mut bytes).context("reading download stream")?;
        Ok(Some((bytes, item["eTag"].as_str().unwrap_or_default().to_owned())))
    }

    pub fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() <= SIMPLE_UPLOAD_LIMIT {
            self.ok(&format!("upload {rel}"), true, |auth| {
                agent()
                    .put(&approot_url(rel, ":/content"))
                    .set("Authorization", auth.unwrap_or_default())
                    .set("Content-Type", "application/octet-stream")
                    .send_bytes(bytes)
            })?;
            return Ok(());
        }

        // Large upload: resumable session, chunked.
        let session_body = r#"{"item":{"@microsoft.graph.conflictBehavior":"replace"}}"#;
        let session: serde_json::Value = match self.ok(&format!("upload session for {rel}"), true, |auth| {
            agent()
                .post(&approot_url(rel, ":/createUploadSession"))
                .set("Authorization", auth.unwrap_or_default())
                .set("Content-Type", "application/json")
                .send_string(session_body)
        })? {
            Some(resp) => serde_json::from_str(&resp.into_string()?)?,
            None => bail!("upload session for {rel}: not found"),
        };
        let upload_url = session["uploadUrl"].as_str().context("upload session missing uploadUrl")?;

        let total = bytes.len();
        let mut offset = 0;
        while offset < total {
            let end = (offset + CHUNK).min(total);
            let range = format!("bytes {}-{}/{}", offset, end - 1, total);
            self.ok(&format!("chunk {range} of {rel}"), false, |_| {
                agent().put(upload_url).set("Content-Range", &range).send_bytes(&bytes[offset..end])
            })?;
            offset = end;
        }
        Ok(())
    }

    /// Small files only (indexes): replaces `rel` only if its eTag is still
    /// `expected`, or creates it only if it doesn't exist (`None`).
    pub fn upload_if(&self, rel: &str, bytes: &[u8], expected: Option<&str>) -> Result<bool> {
        anyhow::ensure!(bytes.len() <= SIMPLE_UPLOAD_LIMIT, "{rel} is too large for a conditional upload");
        let url = match expected {
            Some(_) => approot_url(rel, ":/content"),
            None => approot_url(rel, ":/content?@microsoft.graph.conflictBehavior=fail"),
        };
        let reply = self.send(&format!("upload {rel}"), true, |auth| {
            let mut request = agent()
                .put(&url)
                .set("Authorization", auth.unwrap_or_default())
                .set("Content-Type", "application/octet-stream");
            if let Some(tag) = expected {
                request = request.set("If-Match", tag);
            }
            request.send_bytes(bytes)
        })?;
        match reply {
            Reply::Ok(_) => Ok(true),
            Reply::Changed => Ok(false),
            Reply::NotFound => bail!("upload {rel}: not found"),
        }
    }

    pub fn delete(&self, rel: &str) -> Result<()> {
        self.ok(&format!("delete {rel}"), true, |auth| agent().delete(&approot_url(rel, "")).set("Authorization", auth.unwrap_or_default()).call())?;
        Ok(())
    }
}

/// Adapts `Session` to the `Provider` trait so `Store` can treat OneDrive
/// and LocalFolder interchangeably (D8).
#[derive(Default)]
pub struct OneDriveProvider {
    session: Session,
}

impl OneDriveProvider {
    pub fn new() -> OneDriveProvider {
        OneDriveProvider { session: Session::new() }
    }
}

impl crate::provider::Provider for OneDriveProvider {
    fn exists(&self, rel: &str) -> Result<bool> {
        Ok(self.session.item_get(rel)?.is_some())
    }

    fn download(&self, rel: &str) -> Result<Vec<u8>> {
        self.download_if_exists(rel)?.with_context(|| format!("{rel} not found in cloud"))
    }

    fn upload(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        self.session.upload(rel, bytes)
    }

    fn delete(&self, rel: &str) -> Result<()> {
        self.session.delete(rel)
    }

    fn list(&self, rel: &str) -> Result<Vec<String>> {
        self.session.list_children(rel)
    }

    fn download_if_exists(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.session.download_tagged(rel)?.map(|(bytes, _)| bytes))
    }

    fn download_tagged(&self, rel: &str) -> Result<Option<(Vec<u8>, String)>> {
        self.session.download_tagged(rel)
    }

    fn upload_if(&self, rel: &str, bytes: &[u8], expected: Option<&str>) -> Result<bool> {
        self.session.upload_if(rel, bytes, expected)
    }
}
