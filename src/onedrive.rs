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

pub fn save_tokens(tokens: &Tokens) -> Result<()> {
    let path = token_path()?;
    fs::write(&path, serde_json::to_vec_pretty(tokens)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
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
    let body = match ureq::post(TOKEN_URL).send_form(params) {
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
        Err(err) => return Err(err).context("token endpoint unreachable"),
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

/// Device-code sign-in (Steam Deck Gaming Mode, SSH sessions, …).
pub fn login_device() -> Result<Tokens> {
    let resp = ureq::post(DEVICE_CODE_URL)
        .send_form(&[("client_id", CLIENT_ID), ("scope", SCOPES)])
        .context("requesting device code")?;
    let dc: DeviceCodeResponse = serde_json::from_str(&resp.into_string()?)?;
    match &dc.message {
        Some(message) => println!("{message}"),
        None => println!(
            "Visit {} on any device and enter the code {}",
            dc.verification_uri, dc.user_code
        ),
    }

    let mut interval = dc.interval.max(1);
    // Entra's default device-code lifetime is 900s; fall back to that if the
    // server omits expires_in, so a local guard still exists as a backstop
    // alongside the server's own expired_token error.
    let deadline = if dc.expires_in > 0 { dc.expires_in } else { 900 };
    let mut waited = 0u64;
    let mut last_ping = 0u64;
    loop {
        sleep(Duration::from_secs(interval));
        waited += interval;
        if waited - last_ping >= PING_EVERY_SECS {
            eprintln!("  still waiting for sign-in… ({waited}s elapsed)");
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
pub fn ensure_access_token() -> Result<String> {
    let tokens = load_tokens().context("not signed in — run `yasgm auth` first")?;
    if tokens.expires_at > unix_now() {
        return Ok(tokens.access_token);
    }
    Ok(refresh(tokens)?.access_token)
}

pub fn graph_get(access_token: &str, path: &str) -> Result<serde_json::Value> {
    match ureq::get(&format!("{GRAPH}{path}"))
        .set("Authorization", &format!("Bearer {access_token}"))
        .call()
    {
        Ok(resp) => Ok(serde_json::from_str(&resp.into_string()?)?),
        Err(ureq::Error::Status(code, resp)) => {
            bail!("Graph {path} failed ({code}): {}", resp.into_string().unwrap_or_default())
        }
        Err(err) => Err(err).context("Graph unreachable"),
    }
}

// ---- App-folder file operations ------------------------------------------
// `rel` is a path relative to the app folder, no leading slash.

fn approot_url(rel: &str, suffix: &str) -> String {
    format!("{GRAPH}/me/drive/special/approot:/{rel}{suffix}")
}

fn bearer(access_token: &str) -> String {
    format!("Bearer {access_token}")
}

/// Item metadata, or None if the path doesn't exist.
pub fn item_get(access_token: &str, rel: &str) -> Result<Option<serde_json::Value>> {
    match ureq::get(&approot_url(rel, ""))
        .set("Authorization", &bearer(access_token))
        .call()
    {
        Ok(resp) => Ok(Some(serde_json::from_str(&resp.into_string()?)?)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(ureq::Error::Status(code, resp)) => {
            bail!("Graph get {rel} failed ({code}): {}", resp.into_string().unwrap_or_default())
        }
        Err(err) => Err(err).context("Graph unreachable"),
    }
}

pub fn download(access_token: &str, rel: &str) -> Result<Vec<u8>> {
    let item = item_get(access_token, rel)?
        .with_context(|| format!("{rel} not found in cloud"))?;
    // The pre-authorized download URL avoids auth-header issues on the CDN
    // redirect target.
    let url = item["@microsoft.graph.downloadUrl"]
        .as_str()
        .with_context(|| format!("no download URL for {rel}"))?;
    let resp = ureq::get(url).call().context("downloading content")?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .context("reading download stream")?;
    Ok(bytes)
}

const SIMPLE_UPLOAD_LIMIT: usize = 4_000_000;
// Graph requires chunk sizes in multiples of 320 KiB; 10 MiB qualifies.
const CHUNK: usize = 10_485_760;

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

pub fn upload(access_token: &str, rel: &str, bytes: &[u8]) -> Result<()> {
    if bytes.len() <= SIMPLE_UPLOAD_LIMIT {
        return match ureq::put(&approot_url(rel, ":/content"))
            .set("Authorization", &bearer(access_token))
            .set("Content-Type", "application/octet-stream")
            .send_bytes(bytes)
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(507, _)) => Err(QuotaExceeded.into()),
            Err(ureq::Error::Status(code, resp)) => {
                bail!("upload {rel} failed ({code}): {}", resp.into_string().unwrap_or_default())
            }
            Err(err) => Err(err).context("Graph unreachable"),
        };
    }

    // Large upload: resumable session, chunked.
    let session_body = r#"{"item":{"@microsoft.graph.conflictBehavior":"replace"}}"#;
    let session: serde_json::Value = match ureq::post(&approot_url(rel, ":/createUploadSession"))
        .set("Authorization", &bearer(access_token))
        .set("Content-Type", "application/json")
        .send_string(session_body)
    {
        Ok(resp) => serde_json::from_str(&resp.into_string()?)?,
        Err(ureq::Error::Status(507, _)) => return Err(QuotaExceeded.into()),
        Err(ureq::Error::Status(code, resp)) => {
            bail!("upload session for {rel} failed ({code}): {}", resp.into_string().unwrap_or_default())
        }
        Err(err) => return Err(err).context("Graph unreachable"),
    };
    let upload_url = session["uploadUrl"]
        .as_str()
        .context("upload session missing uploadUrl")?;

    let total = bytes.len();
    let mut offset = 0;
    while offset < total {
        let end = (offset + CHUNK).min(total);
        let range = format!("bytes {}-{}/{}", offset, end - 1, total);
        match ureq::put(upload_url)
            .set("Content-Range", &range)
            .send_bytes(&bytes[offset..end])
        {
            Ok(_) => {}
            Err(ureq::Error::Status(507, _)) => return Err(QuotaExceeded.into()),
            Err(ureq::Error::Status(code, resp)) => {
                bail!("chunk {range} of {rel} failed ({code}): {}", resp.into_string().unwrap_or_default())
            }
            Err(err) => return Err(err).context("Graph unreachable during chunk upload"),
        }
        offset = end;
    }
    Ok(())
}

pub fn delete(access_token: &str, rel: &str) -> Result<()> {
    match ureq::delete(&approot_url(rel, ""))
        .set("Authorization", &bearer(access_token))
        .call()
    {
        Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
        Err(ureq::Error::Status(code, resp)) => {
            bail!("delete {rel} failed ({code}): {}", resp.into_string().unwrap_or_default())
        }
        Err(err) => Err(err).context("Graph unreachable"),
    }
}
