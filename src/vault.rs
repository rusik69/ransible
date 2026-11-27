//! Ansible Vault (format 1.1 / 1.2, AES256) encryption and decryption.
use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{Result, anyhow, bail};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::OnceLock;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;
const HEADER: &str = "$ANSIBLE_VAULT";

/// (label, password) pairs; the first entry is the default for encryption.
static PASSWORDS: OnceLock<Vec<(Option<String>, String)>> = OnceLock::new();

/// Configure the global vault password (from --vault-password-file / --ask-vault-pass / env).
pub fn set_passwords(p: Vec<(Option<String>, String)>) {
    let _ = PASSWORDS.set(p);
}

/// Password to encrypt with: the one labelled `label`, else the default.
pub fn password_for(label: Option<&str>) -> Option<String> {
    let all = PASSWORDS.get()?;
    label
        .and_then(|l| all.iter().find(|(lab, _)| lab.as_deref() == Some(l)))
        .or_else(|| all.first())
        .map(|(_, p)| p.clone())
}

/// Vault label from a `$ANSIBLE_VAULT;1.2;AES256;label` header.
pub fn header_label(text: &str) -> Option<String> {
    let first = text.trim_start().lines().next()?;
    let parts: Vec<&str> = first.trim().split(';').collect();
    (parts.len() >= 4).then(|| parts[3].to_string())
}

pub fn load_password_file(path: &str) -> Result<String> {
    let meta = std::fs::metadata(path).map_err(|e| anyhow!("vault password file {path}: {e}"))?;
    #[cfg(not(unix))]
    let _ = &meta;
    #[cfg(unix)]
    let exec = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let exec = false;
    let raw = if exec {
        let o = std::process::Command::new(path).output()?;
        if !o.status.success() {
            bail!("vault password script {path} failed");
        }
        String::from_utf8_lossy(&o.stdout).into_owned()
    } else {
        std::fs::read_to_string(path)?
    };
    Ok(raw.lines().next().unwrap_or("").trim_end().to_string())
}

pub fn is_vault(data: &[u8]) -> bool {
    data.starts_with(HEADER.as_bytes())
}

fn derive(password: &[u8], salt: &[u8]) -> ([u8; 32], [u8; 32], [u8; 16]) {
    let mut out = [0u8; 80];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, 10_000, &mut out);
    let mut k1 = [0u8; 32];
    let mut k2 = [0u8; 32];
    let mut iv = [0u8; 16];
    k1.copy_from_slice(&out[..32]);
    k2.copy_from_slice(&out[32..64]);
    iv.copy_from_slice(&out[64..]);
    (k1, k2, iv)
}

pub fn decrypt(text: &str, password: &str) -> Result<Vec<u8>> {
    let mut lines = text.trim().lines();
    let header = lines.next().ok_or_else(|| anyhow!("empty vault"))?;
    let parts: Vec<&str> = header.trim().split(';').collect();
    if parts.len() < 3 || parts[0] != HEADER || parts[2] != "AES256" {
        bail!("unsupported vault header: {header}");
    }
    let body: String = lines.map(|l| l.trim()).collect();
    let inner = hex::decode(body).map_err(|e| anyhow!("vault body is not hex: {e}"))?;
    let inner = String::from_utf8(inner).map_err(|_| anyhow!("bad vault payload"))?;
    let mut it = inner.splitn(3, '\n');
    let (salt, mac, ct) = (it.next(), it.next(), it.next());
    let (Some(salt), Some(mac), Some(ct)) = (salt, mac, ct) else {
        bail!("malformed vault payload")
    };
    let (salt, mac, ct) = (hex::decode(salt)?, hex::decode(mac)?, hex::decode(ct)?);
    let (k1, k2, iv) = derive(password.as_bytes(), &salt);
    let mut h = <Hmac<Sha256> as Mac>::new_from_slice(&k2).unwrap();
    h.update(&ct);
    h.verify_slice(&mac).map_err(|_| {
        anyhow!("Decryption failed (no vault secrets were found that could decrypt)")
    })?;
    let mut buf = ct;
    Aes256Ctr::new(&k1.into(), &iv.into()).apply_keystream(&mut buf);
    let pad = *buf.last().ok_or_else(|| anyhow!("empty plaintext"))? as usize;
    if pad == 0 || pad > 16 || pad > buf.len() {
        bail!("bad padding");
    }
    buf.truncate(buf.len() - pad);
    Ok(buf)
}

pub fn encrypt(data: &[u8], password: &str, label: Option<&str>) -> Result<String> {
    use rand::RngCore;
    let mut salt = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut salt);
    let (k1, k2, iv) = derive(password.as_bytes(), &salt);
    let pad = 16 - data.len() % 16;
    let mut buf = data.to_vec();
    buf.extend(std::iter::repeat_n(pad as u8, pad));
    Aes256Ctr::new(&k1.into(), &iv.into()).apply_keystream(&mut buf);
    let mut h = <Hmac<Sha256> as Mac>::new_from_slice(&k2).unwrap();
    h.update(&buf);
    let mac = h.finalize().into_bytes();
    let inner = format!(
        "{}\n{}\n{}",
        hex::encode(salt),
        hex::encode(mac),
        hex::encode(&buf)
    );
    let body = hex::encode(inner);
    let header = match label {
        Some(l) => format!("{HEADER};1.2;AES256;{l}"),
        None => format!("{HEADER};1.1;AES256"),
    };
    let mut out = header + "\n";
    for chunk in body.as_bytes().chunks(80) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    Ok(out)
}

/// Decrypt vault text with the configured password.
pub fn decrypt_auto(text: &str) -> Result<Vec<u8>> {
    let all = PASSWORDS.get().filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("Attempting to decrypt but no vault secrets found (use --vault-password-file, --vault-id or --ask-vault-pass)"))?;
    // try the password with the matching label first, then the rest
    let label = header_label(text);
    let mut order: Vec<&(Option<String>, String)> = all
        .iter()
        .filter(|(l, _)| l.is_some() && *l == label)
        .collect();
    order.extend(all.iter().filter(|(l, _)| !(l.is_some() && *l == label)));
    let mut last = None;
    for (_, pw) in order {
        match decrypt(text, pw) {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("Decryption failed")))
}

/// Read a file, transparently decrypting vaulted content.
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    if is_vault(&data) {
        decrypt_auto(&String::from_utf8_lossy(&data))
    } else {
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_and_headers() {
        let enc = encrypt(b"x", "pw", Some("prod")).unwrap();
        assert!(enc.starts_with("$ANSIBLE_VAULT;1.2;AES256;prod\n"));
        assert_eq!(header_label(&enc).as_deref(), Some("prod"));
        assert_eq!(header_label(&encrypt(b"x", "pw", None).unwrap()), None);
        assert_eq!(decrypt(&enc, "pw").unwrap(), b"x");
    }

    #[test]
    fn roundtrip() {
        let enc = encrypt(b"secret: 42\n", "pw", None).unwrap();
        assert!(enc.starts_with("$ANSIBLE_VAULT;1.1;AES256\n"));
        assert_eq!(decrypt(&enc, "pw").unwrap(), b"secret: 42\n");
        assert!(decrypt(&enc, "bad").is_err());
    }
}
