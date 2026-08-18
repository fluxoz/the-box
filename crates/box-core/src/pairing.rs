//! The pairing-code format, owned in one place.
//!
//! Three separate programs touch a pairing code — the browser installer, the
//! console installer, and the daemon that redeems it — and they must agree
//! byte-for-byte on what gets hashed. If normalisation drifts between them,
//! codes stop matching and the failure looks like "the code is wrong", which is
//! the least debuggable outcome there is. So the format lives here and everyone
//! calls into it.
//!
//! A code is 80 bits of `/dev/urandom` in Crockford base32, grouped for humans:
//! `XXXX-XXXX-XXXX-XXXX`.
//!
//! 80 bits, not 40, because the SHA-256 of a code travels inside the install
//! orders — which land in shell history, process tables, screenshots and
//! support threads. 2^40 candidates fall to a commodity GPU in under a minute,
//! so publishing that hash used to publish the code itself.
//!
//! Crockford's alphabet omits I, L, O and U: nothing can be misread as
//! something else, and no code can accidentally spell a word.

use std::io::Read;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A fresh pairing code, grouped for reading aloud and typing.
pub fn generate() -> std::io::Result<String> {
    let mut buf = [0u8; 10]; // 80 bits = exactly 16 base32 symbols, no padding
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    let mut bits = 0u32;
    let mut nbits = 0u32;
    let mut sym = String::with_capacity(16);
    for b in buf {
        bits = (bits << 8) | u32::from(b);
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            sym.push(CROCKFORD[((bits >> nbits) & 31) as usize] as char);
        }
    }
    Ok(sym
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-"))
}

/// Fold a typed code back to the form that was hashed: drop separators,
/// uppercase, and apply Crockford's confusable rules. Someone reading a code
/// off a screen onto a phone keyboard must land on the same bytes.
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect()
}

/// What goes in the orders: the hash of the canonical form. The code itself
/// never leaves the machine that generated it.
pub fn hash(code: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(normalize(code).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_typeable_and_normalise_the_way_people_type() {
        let code = generate().unwrap();
        assert_eq!(code.len(), 19, "16 symbols + 3 dashes: {code}");
        assert_eq!(code.matches('-').count(), 3);
        assert!(
            code.chars()
                .all(|c| c == '-' || CROCKFORD.contains(&(c as u8))),
            "unexpected symbol in {code}"
        );
        // Never emits the confusable letters, so reading one aloud is safe.
        assert!(!code.contains(['I', 'L', 'O', 'U']));

        // The same code typed carelessly still matches.
        let canon = hash(&code);
        assert_eq!(hash(&code.to_lowercase()), canon);
        assert_eq!(hash(&code.replace('-', "")), canon);
        assert_eq!(hash(&format!("  {code}  ")), canon);
        assert_eq!(hash(&code.replace('0', "O")), canon, "O reads as zero");
        assert_eq!(hash(&code.replace('1', "l")), canon, "l reads as one");

        assert_ne!(generate().unwrap(), generate().unwrap());
    }

    #[test]
    fn eighty_bits_of_entropy() {
        // 16 base32 symbols carry 80 bits. The old 40-bit code was the reason
        // publishing the orders hash effectively published the code.
        let stripped = normalize(&generate().unwrap());
        assert_eq!(stripped.len() * 5, 80);
    }
}
