//! Pairing codes: the short human-typed secret that bootstraps trust (§2.1).
//!
//! The brain mints a code, the owner reads it to the node operator, the node
//! sends it back over HTTPS with its fresh public key. Only the hash of the
//! code is ever stored, and comparisons run in constant time — the code is a
//! shared secret, so neither the store nor the compare may leak it.

// `try_fill_bytes` lives on this trait in rand 0.9.
use rand::TryRngCore as _;
use sha2::{Digest, Sha256};
use thiserror::Error;

use swarmbot_contracts::PAIRING_HASH_PREFIX;

/// The Crockford base32 alphabet (§2.1): no `I`, `L`, `O`, `U`, so a code read
/// aloud over the phone cannot be misheard into a different valid code.
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A pairing code is 8 characters (§2.1).
pub const CODE_LEN: usize = 8;

/// Errors while minting or normalizing a pairing code.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PairError {
    #[error("the system RNG failed: {reason}")]
    Rng { reason: String },

    #[error("pairing code is empty")]
    Empty,

    #[error("pairing code contains {ch:?}, which is not in the Crockford base32 alphabet")]
    InvalidChar { ch: char },
}

/// Mint a fresh pairing code from the operating system's CSPRNG.
///
/// Returns both shapes because both are needed: `display` (`ABCD-EFGH`) is
/// what the owner reads and types, `raw` (`ABCDEFGH`) is what gets hashed.
/// Never a seeded or deterministic RNG — the code is a secret.
pub fn mint_code() -> Result<PairCode, PairError> {
    let mut bytes = [0u8; CODE_LEN];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| PairError::Rng {
            reason: e.to_string(),
        })?;
    // The alphabet has exactly 32 entries and 256 % 32 == 0, so taking the
    // low 5 bits of each random byte is uniform — no rejection sampling
    // needed, no modulo bias.
    let raw: String = bytes
        .iter()
        .map(|b| CODE_ALPHABET[(b & 0b1_1111) as usize] as char)
        .collect();
    Ok(PairCode {
        display: format_display(&raw),
        raw,
    })
}

/// A minted pairing code in both shapes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairCode {
    /// `ABCD-EFGH` — what the owner sees and types.
    pub display: String,
    /// `ABCDEFGH` — what gets hashed and compared.
    pub raw: String,
}

impl PairCode {
    /// `sha256("<prefix>|<raw>")`, hex — the value the brain stores.
    pub fn hash(&self) -> String {
        hash_code(&self.raw)
    }
}

/// Insert the dash that makes a code readable: `ABCDEFGH` → `ABCD-EFGH`.
pub fn format_display(raw: &str) -> String {
    let (head, tail) = raw.split_at(raw.len().min(4));
    if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head}-{tail}")
    }
}

/// Normalize user input to the canonical raw form (§2.1): upper-case, spaces
/// and dashes removed, `I`/`L` → `1` and `O` → `0` (the look-alikes Crockford
/// excludes), anything else outside the alphabet refused.
pub fn normalize_code(input: &str) -> Result<String, PairError> {
    let mut out = String::with_capacity(input.len());
    for ch in input.to_ascii_uppercase().chars() {
        match ch {
            ' ' | '-' => {}
            'I' | 'L' => out.push('1'),
            'O' => out.push('0'),
            c if CODE_ALPHABET.contains(&(c as u8)) => out.push(c),
            c => {
                return Err(PairError::InvalidChar { ch: c });
            }
        }
    }
    if out.is_empty() {
        return Err(PairError::Empty);
    }
    Ok(out)
}

/// `sha256("<PAIRING_HASH_PREFIX>|<code>")`, hex (§2.1). The prefix binds the
/// hash to this product so a code database from something else never matches.
pub fn hash_code(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PAIRING_HASH_PREFIX.as_bytes());
    hasher.update(b"|");
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

/// Compare two (hash) strings without short-circuiting on the first differing
/// byte, so the comparison does not leak *where* two hashes diverge.
///
/// Every byte of both inputs is always visited — including when the lengths
/// differ (the length difference is folded into the accumulator up front, and
/// the loop still runs over the longer input).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn minted_codes_use_the_crockford_alphabet_and_length() {
        for _ in 0..64 {
            let code = mint_code().unwrap();
            assert_eq!(code.raw.len(), CODE_LEN);
            assert!(
                code.raw.bytes().all(|b| CODE_ALPHABET.contains(&b)),
                "raw {:?} outside alphabet",
                code.raw
            );
            assert_eq!(code.display.len(), CODE_LEN + 1);
            assert_eq!(code.display.as_bytes()[4], b'-');
            assert_eq!(
                format!("{}{}", &code.display[..4], &code.display[5..]),
                code.raw
            );
            assert_eq!(code.display, format_display(&code.raw));
        }
        // 32^8 possibilities: two mints colliding is not a real risk.
        assert_ne!(mint_code().unwrap().raw, mint_code().unwrap().raw);
    }

    #[test]
    fn normalization_uppercases_strips_and_maps_lookalikes() {
        assert_eq!(normalize_code("abcd-efgh").unwrap(), "ABCDEFGH");
        assert_eq!(normalize_code("ABCD EFGH").unwrap(), "ABCDEFGH");
        assert_eq!(normalize_code("  abcd efgh  ").unwrap(), "ABCDEFGH");
        assert_eq!(normalize_code("i l o").unwrap(), "110");
        assert_eq!(normalize_code("ILO").unwrap(), "110");
        assert_eq!(normalize_code("il-o").unwrap(), "110");
        // U is not in the Crockford alphabet either.
        assert_eq!(
            normalize_code("abcu"),
            Err(PairError::InvalidChar { ch: 'U' })
        );
        assert_eq!(
            normalize_code("abc!"),
            Err(PairError::InvalidChar { ch: '!' })
        );
        assert_eq!(normalize_code(""), Err(PairError::Empty));
        assert_eq!(normalize_code(" - "), Err(PairError::Empty));
    }

    #[test]
    fn hashing_is_stable_prefixed_and_hex() {
        let hash = hash_code("ABCD1234");
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(hash, hash_code("ABCD1234"));
        assert_ne!(hash, hash_code("ABCD1235"));
        // Pins the exact formula: sha256("<PAIRING_HASH_PREFIX>|<code>").
        let expected = hex::encode(Sha256::digest(format!("{PAIRING_HASH_PREFIX}|ABCD1234")));
        assert_eq!(hash, expected);
        // The minted shape hashes the raw form.
        let code = mint_code().unwrap();
        assert_eq!(code.hash(), hash_code(&code.raw));
        assert_eq!(
            code.hash(),
            hash_code(&normalize_code(&code.display).unwrap())
        );
    }

    #[test]
    fn constant_time_compare_does_not_short_circuit() {
        // Equal pairs.
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq("abcdef", "abcdef"));
        // Same length, mismatch at the first byte.
        assert!(!constant_time_eq("zbcdef", "abcdef"));
        // Same length, mismatch at the last byte.
        assert!(!constant_time_eq("abcdeg", "abcdef"));
        // Different lengths (prefix, suffix, disjoint).
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abcd", "abc"));
        assert!(!constant_time_eq("", "a"));
        assert!(!constant_time_eq("xyz", "abc"));
        // Long hashes that differ only at the end must still be false.
        let a = "0".repeat(64);
        let mut b = "0".repeat(64);
        b.replace_range(63..64, "1");
        assert!(!constant_time_eq(&a, &b));
        assert!(constant_time_eq(&a, &"0".repeat(64)));
    }
}
