//! Cloud setup: `auth`, `status`, `provider`, `autostart`.


use anyhow::{Context, Result, ensure};

use crate::args::*;
use crate::config;
use crate::ctx::*;
use crate::{autostart, lock, onedrive};

pub(crate) fn auth(args: &[String]) -> Result<()> {
    if let config::ProviderConfig::Local { path } = config::Config::load().provider {
        if flag(args, "--json") {
            println!("{}", serde_json::json!({"event": "not_needed", "detail": format!("cloud provider is a local folder ({})", path.display())}));
        } else {
            println!("cloud provider is a local folder ({}); no sign-in needed.", path.display());
        }
        return Ok(());
    }
    if flag(args, "--device") && flag(args, "--json") {
        return auth_device_json();
    }
    let tokens = if flag(args, "--device") {
        onedrive::login_device(&|event| match event {
            onedrive::DeviceEvent::Code { user_code, verification_uri, message, .. } => {
                match message {
                    Some(message) => println!("{message}"),
                    None => println!("Visit {verification_uri} on any device and enter the code {user_code}"),
                }
                // Scan with a phone instead of typing the link.
                if let Ok(code) = qrcode::QrCode::new(verification_uri.as_bytes()) {
                    println!(
                        "\n{}\n",
                        code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build()
                    );
                }
            }
            onedrive::DeviceEvent::Waiting { elapsed } => eprintln!("  still waiting for sign-in… ({elapsed}s elapsed)"),
        })?
    } else {
        onedrive::login_interactive()?
    };
    onedrive::save_tokens(&tokens)?;
    connect_onedrive()?;
    println!("Sign-in successful; tokens cached.\n");
    println!("{}", cloud_summary()?);
    Ok(())
}

/// Signing in is choosing OneDrive: a disconnected install becomes a
/// OneDrive one (a LocalFolder install never gets here — `auth` refuses).
pub(crate) fn connect_onedrive() -> Result<()> {
    let mut cfg = config::Config::load();
    if matches!(cfg.provider, config::ProviderConfig::None) {
        cfg.provider = config::ProviderConfig::Onedrive;
        cfg.save()?;
    }
    Ok(())
}

/// `auth --device --json`: one JSON object per line, for a UI (the Decky
/// plugin shows the code and a QR of the URL): `{"event":"code",
/// "user_code","verification_uri","expires_in"}`, then `{"event":"waiting",
/// "elapsed"}` every 30 s, then `{"event":"signed_in","detail"}` or
/// `{"event":"error","message"}` (exit status 1).
pub(crate) fn auth_device_json() -> Result<()> {
    let emit = |value: serde_json::Value| {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    };
    let result = onedrive::login_device(&|event| match event {
        onedrive::DeviceEvent::Code { user_code, verification_uri, expires_in, .. } => emit(serde_json::json!({
            "event": "code",
            "user_code": user_code,
            "verification_uri": verification_uri,
            "expires_in": expires_in,
        })),
        onedrive::DeviceEvent::Waiting { elapsed } => emit(serde_json::json!({"event": "waiting", "elapsed": elapsed})),
    })
    .and_then(|tokens| {
        onedrive::save_tokens(&tokens)?;
        connect_onedrive()?;
        cloud_summary()
    });
    match result {
        Ok(detail) => {
            emit(serde_json::json!({"event": "signed_in", "detail": detail}));
            Ok(())
        }
        Err(err) => {
            emit(serde_json::json!({"event": "error", "message": format!("{err:#}")}));
            std::process::exit(1);
        }
    }
}

/// One line describing the OneDrive app folder, proving the token works.
pub(crate) fn cloud_summary() -> Result<String> {
    // Note: the AppFolder scope cannot read /me/drive (drive-wide metadata is
    // out of bounds — by design). The app folder itself is all we may touch;
    // the first request auto-creates it (Apps/YASGM).
    let approot = onedrive::Session::new().graph_get("/me/drive/special/approot")?;
    let name = approot["name"].as_str().unwrap_or("?");
    let parent = approot["parentReference"]["path"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches(':')
        .rsplit(':')
        .next()
        .unwrap_or("");
    let owner = approot["createdBy"]["user"]["displayName"].as_str().unwrap_or("?");
    let size = approot["size"].as_u64().unwrap_or(0);
    Ok(format!("App folder ready: {parent}/{name} (owner: {owner}, current size: {})", human_bytes(size)))
}

/// `status [--json]`: is the configured provider reachable and writable,
/// and is the watch daemon running. With `--json` a failed check is
/// reported in the output (`"ok": false, "error"`) rather than as an error.
pub(crate) fn status(args: &[String]) -> Result<()> {
    let cfg = config::Config::load();
    if !flag(args, "--json") {
        println!("{}", check_provider(&cfg.provider)?);
        return Ok(());
    }
    let check = check_provider(&cfg.provider);
    let daemon = lock::daemon_status()?;
    let (provider, signed_in) = match &cfg.provider {
        config::ProviderConfig::Onedrive => {
            (serde_json::json!({"type": "onedrive"}), serde_json::json!(onedrive::load_tokens().is_some()))
        }
        config::ProviderConfig::Local { path } => {
            (serde_json::json!({"type": "local", "path": path}), serde_json::Value::Null)
        }
        config::ProviderConfig::None => (serde_json::json!({"type": "none"}), serde_json::Value::Null),
    };
    println!(
        "{}",
        serde_json::json!({
            "provider": provider,
            "signed_in": signed_in,
            "ok": check.is_ok(),
            "detail": check.as_ref().ok(),
            "error": check.as_ref().err().map(|err| format!("{err:#}")),
            "daemon": {"running": daemon.is_some(), "pid": daemon.flatten()},
        })
    );
    Ok(())
}

/// `Ok(summary)` if the configured provider is reachable and writable.
pub(crate) fn check_provider(provider: &config::ProviderConfig) -> Result<String> {
    match provider {
        config::ProviderConfig::None => anyhow::bail!(NOT_CONNECTED),
        config::ProviderConfig::Onedrive => cloud_summary(),
        config::ProviderConfig::Local { path } => {
            let meta = std::fs::metadata(&path)
                .with_context(|| format!("{} not accessible", path.display()))?;
            ensure!(meta.is_dir(), "{} is not a directory", path.display());
            let probe = path.join(".yasgm-write-test");
            std::fs::write(&probe, b"ok")
                .with_context(|| format!("{} is not writable", path.display()))?;
            let _ = std::fs::remove_file(&probe);
            Ok(format!("Local folder provider ready: {}", path.display()))
        }
    }
}

/// Select or inspect the cloud provider (D8).
pub(crate) fn provider_cmd(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    let mut cfg = config::Config::load();
    match pos.first().map(String::as_str) {
        Some("onedrive") => {
            cfg.provider = config::ProviderConfig::Onedrive;
            cfg.save()?;
            println!("cloud provider set to OneDrive");
        }
        Some("disconnect") => {
            let was = std::mem::replace(&mut cfg.provider, config::ProviderConfig::None);
            cfg.save()?;
            let detail = match was {
                config::ProviderConfig::Onedrive => {
                    onedrive::delete_tokens()?;
                    "signed out of OneDrive on this device. Your saves in OneDrive are untouched; to also revoke \
                     YASGM's access, visit https://account.live.com/consent/Manage"
                        .to_owned()
                }
                config::ProviderConfig::Local { path } => {
                    format!("stopped using the folder {} (its contents are untouched)", path.display())
                }
                config::ProviderConfig::None => "no cloud provider was connected".to_owned(),
            };
            if flag(args, "--json") {
                println!("{}", serde_json::json!({"type": "none", "detail": detail}));
            } else {
                println!("disconnected: {detail}");
            }
        }
        Some("local") => {
            let raw = pos.get(1).context("usage: yasgm provider local <path>")?;
            std::fs::create_dir_all(raw).with_context(|| format!("creating {raw}"))?;
            let path = std::fs::canonicalize(raw)?;
            cfg.provider = config::ProviderConfig::Local { path: path.clone() };
            cfg.save()?;
            println!("cloud provider set to local folder: {}", path.display());
        }
        None | Some("status") => {
            if flag(args, "--json") {
                let json = match &cfg.provider {
                    config::ProviderConfig::Onedrive => serde_json::json!({"type": "onedrive"}),
                    config::ProviderConfig::Local { path } => {
                        serde_json::json!({"type": "local", "path": path})
                    }
                    config::ProviderConfig::None => serde_json::json!({"type": "none"}),
                };
                println!("{}", serde_json::to_string(&json)?);
            } else {
                match &cfg.provider {
                    config::ProviderConfig::Onedrive => println!("cloud provider: OneDrive"),
                    config::ProviderConfig::Local { path } => {
                        println!("cloud provider: local folder at {}", path.display())
                    }
                    config::ProviderConfig::None => println!("cloud provider: none (disconnected)"),
                }
            }
        }
        Some(other) => {
            anyhow::bail!("usage: yasgm provider [onedrive|local <path>|disconnect|status] (got {other:?})")
        }
    }
    Ok(())
}

/// Opt-in autostart at login (D11).
pub(crate) fn autostart_cmd(args: &[String]) -> Result<()> {
    match positionals(args).first().map(String::as_str) {
        Some("on") => println!("{}", autostart::enable()?),
        Some("off") => println!("{}", autostart::disable()?),
        None | Some("status") => {
            let detail = autostart::status()?;
            if flag(args, "--json") {
                println!(
                    "{}",
                    serde_json::json!({"enabled": detail.is_some(), "detail": detail})
                );
            } else {
                match detail {
                    Some(detail) => println!("autostart: on ({detail})"),
                    None => println!("autostart: off"),
                }
            }
        }
        Some(other) => anyhow::bail!("usage: yasgm autostart [on|off|status] (got {other:?})"),
    }
    Ok(())
}
