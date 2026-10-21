//! Release signatures (D20/D21). The desktop updater checks its downloads
//! with the public key in `tauri.conf.json`; the Deck plugin's self-update
//! checks the plugin zip with the same key through `yasgm verify-update`
//! (the bundled binary is the trusted code on the Deck). A GitHub-provided
//! SHA-256 only proves the download wasn't corrupted, not who made it.

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use minisign_verify::{PublicKey, Signature};

/// One source of truth for the key: the updater config.
const TAURI_CONF: &str = include_str!("../ui/src-tauri/tauri.conf.json");

fn public_key() -> Result<PublicKey> {
    let conf: serde_json::Value = serde_json::from_str(TAURI_CONF).context("parsing tauri.conf.json")?;
    let encoded = conf["plugins"]["updater"]["pubkey"].as_str().context("no updater pubkey in tauri.conf.json")?;
    let text = String::from_utf8(STANDARD.decode(encoded)?)?;
    PublicKey::decode(&text).map_err(|e| anyhow!("updater public key: {e}"))
}

/// Checks `data` against a `.sig` file as `tauri signer sign` writes it
/// (base64 of a minisign signature).
pub fn verify(data: &[u8], sig_file: &str) -> Result<()> {
    let text = String::from_utf8(STANDARD.decode(sig_file.trim()).context("signature isn't base64")?)?;
    let signature = Signature::decode(&text).map_err(|e| anyhow!("malformed signature: {e}"))?;
    public_key()?.verify(data, &signature, false).map_err(|e| anyhow!("signature doesn't match: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Signed with the release key (`tauri signer sign`).
    const SIGNED: &[u8] = include_bytes!("testdata/signed.txt");
    const SIG: &str = include_str!("testdata/signed.txt.sig");

    #[test]
    fn accepts_the_signed_file_and_rejects_changes() {
        verify(SIGNED, SIG).expect("valid signature");
        let mut tampered = SIGNED.to_vec();
        tampered[0] ^= 1;
        assert!(verify(&tampered, SIG).is_err());
        assert!(verify(SIGNED, "not a signature").is_err());
    }
}
