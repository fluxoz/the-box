//! Encrypting secrets with age (for agenix). boxd encrypts each secret to the
//! box's own SSH host key — so it decrypts unattended at boot — plus the
//! operator's key(s), so the operator can decrypt, re-key, and migrate to a new
//! box. The ciphertext `.age` files live in the config repo; agenix decrypts
//! them at runtime to `/run/agenix`. boxd only ever writes ciphertext; it never
//! holds a decryption identity.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// The recipients every secret is encrypted to: the box's host public key plus
/// every operator key. age accepts ssh ed25519/rsa public keys as recipients
/// directly, so these are the keys the box already has.
pub fn recipients() -> Result<Vec<String>> {
    recipients_from(
        Path::new("/etc/ssh/ssh_host_ed25519_key.pub"),
        Path::new("/etc/box/authorized_keys"),
    )
}

/// Testable core: read the host public key and the operator authorized_keys.
pub fn recipients_from(host_key_pub: &Path, authorized_keys: &Path) -> Result<Vec<String>> {
    let mut r: Vec<String> = Vec::new();
    let mut push = |line: &str| {
        let l = line.trim();
        if is_ssh_key(l) && !r.iter().any(|e| e == l) {
            r.push(l.to_string());
        }
    };
    if let Ok(k) = std::fs::read_to_string(host_key_pub) {
        push(&k);
    }
    if let Ok(text) = std::fs::read_to_string(authorized_keys) {
        for line in text.lines() {
            push(line);
        }
    }
    if r.is_empty() {
        bail!("no age recipients: the box needs its host key or an operator key to encrypt a secret to");
    }
    Ok(r)
}

fn is_ssh_key(line: &str) -> bool {
    [
        "ssh-ed25519",
        "ssh-rsa",
        "sk-ssh-ed25519@openssh.com",
        "ecdsa-sha2-",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

/// Encrypt `value` to `recipients`, writing the `.age` ciphertext to `out`.
pub fn encrypt(value: &str, recipients: &[String], out: &Path) -> Result<()> {
    let mut cmd = Command::new("age");
    for r in recipients {
        cmd.args(["-r", r]);
    }
    cmd.arg("-o").arg(out).stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("running age (is it installed and on PATH?)")?;
    child
        .stdin
        .take()
        .context("age stdin")?
        .write_all(value.as_bytes())?;
    if !child.wait()?.success() {
        bail!("age encryption failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn collects_host_and_operator_keys() {
        let tmp = TempDir::new().unwrap();
        let host = tmp.path().join("host.pub");
        let auth = tmp.path().join("authorized_keys");
        std::fs::write(&host, "ssh-ed25519 AAAAhost box\n").unwrap();
        std::fs::write(
            &auth,
            "ssh-ed25519 AAAAop1 me\n# a comment, not a key\nssh-rsa AAAAop2 other\n",
        )
        .unwrap();

        let r = recipients_from(&host, &auth).unwrap();
        assert_eq!(r.len(), 3, "host + two operator keys, comment ignored");
        assert!(r.iter().any(|k| k.contains("AAAAhost")));
        assert!(r.iter().any(|k| k.contains("AAAAop1")));
        assert!(r.iter().any(|k| k.contains("AAAAop2")));

        // No keys anywhere is an error, not an empty recipient set.
        assert!(recipients_from(&tmp.path().join("x"), &tmp.path().join("y")).is_err());
    }
}
