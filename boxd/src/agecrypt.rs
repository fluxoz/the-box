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
    // Overridable so a test (or an unusual deployment) can point at other key
    // files; a real box uses the well-known SSH host key + operator keys.
    let host = std::env::var("BOX_HOST_KEY_PUB")
        .unwrap_or_else(|_| "/etc/ssh/ssh_host_ed25519_key.pub".into());
    let auth =
        std::env::var("BOX_AUTHORIZED_KEYS").unwrap_or_else(|_| "/etc/box/authorized_keys".into());
    recipients_from(Path::new(&host), Path::new(&auth))
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
        // Native age recipients are valid too (a box may carry an age key).
        "age1",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

/// Encrypt `value` to `recipients`, writing the `.age` ciphertext to `out`.
pub fn encrypt(value: &str, recipients: &[String], out: &Path) -> Result<()> {
    encrypt_bytes(value.as_bytes(), recipients, out)
}

/// Encrypt raw bytes (used when re-keying an existing secret).
pub fn encrypt_bytes(value: &[u8], recipients: &[String], out: &Path) -> Result<()> {
    let mut cmd = Command::new("age");
    for r in recipients {
        cmd.args(["-r", r]);
    }
    cmd.arg("-o").arg(out).stdin(Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("running age (is it installed and on PATH?)")?;
    child.stdin.take().context("age stdin")?.write_all(value)?;
    if !child.wait()?.success() {
        bail!("age encryption failed");
    }
    Ok(())
}

/// Decrypt a `.age` file with an identity (an SSH or age private key). Used on
/// restore, where the operator's key decrypts secrets the fresh box can't yet.
pub fn decrypt(age_file: &Path, identity: &Path) -> Result<Vec<u8>> {
    let out = Command::new("age")
        .arg("-d")
        .arg("-i")
        .arg(identity)
        .arg(age_file)
        .output()
        .context("running age -d")?;
    if !out.status.success() {
        bail!(
            "age decryption failed for {}: {}",
            age_file.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Re-encrypt a secret to a new recipient set, decrypting with `identity`. On
/// restore the fresh box isn't a recipient of the cloned secrets, so it re-keys
/// each one (with the operator's key) to `[new host key + operator]`, after
/// which it decrypts them unattended forever.
pub fn rekey(age_file: &Path, identity: &Path, recipients: &[String]) -> Result<()> {
    let plaintext = decrypt(age_file, identity)?;
    encrypt_bytes(&plaintext, recipients, age_file)
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

    fn age_available() -> bool {
        Command::new("age")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            && Command::new("age-keygen")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    }

    // Generate an age identity; return (identity_path, recipient_string).
    fn keygen(dir: &Path, name: &str) -> (std::path::PathBuf, String) {
        let key = dir.join(name);
        let out = Command::new("age-keygen").arg("-o").arg(&key).output().unwrap();
        assert!(out.status.success());
        let pub_out = Command::new("age-keygen").arg("-y").arg(&key).output().unwrap();
        let recipient = String::from_utf8_lossy(&pub_out.stdout).trim().to_string();
        (key, recipient)
    }

    #[test]
    fn rekey_swaps_the_recipient_set() {
        if !age_available() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (op_id, op_rcpt) = keygen(tmp.path(), "operator");
        let (box1_id, box1_rcpt) = keygen(tmp.path(), "box1");
        let (box2_id, box2_rcpt) = keygen(tmp.path(), "box2");

        // box1 encrypts a secret to [box1 host + operator], as deploy() does.
        let secret = tmp.path().join("db-env.age");
        encrypt(
            "POSTGRES_PASSWORD=hunter2",
            &[box1_rcpt.clone(), op_rcpt.clone()],
            &secret,
        )
        .unwrap();

        // box2 (fresh, not a recipient) cannot read it yet.
        assert!(decrypt(&secret, &box2_id).is_err());

        // On restore, box2 re-keys with the operator's key to [box2 host + operator].
        rekey(&secret, &op_id, &[box2_rcpt.clone(), op_rcpt.clone()]).unwrap();

        // Now box2 decrypts unattended, the operator still can, and the old box can't.
        assert_eq!(decrypt(&secret, &box2_id).unwrap(), b"POSTGRES_PASSWORD=hunter2");
        assert_eq!(decrypt(&secret, &op_id).unwrap(), b"POSTGRES_PASSWORD=hunter2");
        assert!(decrypt(&secret, &box1_id).is_err());
    }
}
