//! Named API keys and pairing codes.
//!
//! The server's own key (`api-key`) always works. Besides it, each person or device
//! (a Runo install, a laptop's CLI) can get a key of its own, by name, which can be
//! revoked without touching the others. Only a hash of each key is stored.
//!
//! Pairing hands out such a key for a short one-time code, so connecting Runo or
//! another computer is "type this code" instead of copying a URL and a long key.

use crate::paths::Paths;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

/// A key as clients see it: never the key or its hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyInfo {
    pub id: String,
    pub name: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredKey {
    #[serde(flatten)]
    info: KeyInfo,
    /// SHA-256 of the key, hex.
    hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KeyFile {
    keys: Vec<StoredKey>,
}

pub fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn load_file(paths: &Paths) -> KeyFile {
    std::fs::read_to_string(paths.keys_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_file(paths: &Paths, file: &KeyFile) -> Result<()> {
    paths.ensure()?;
    let path = paths.keys_file();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(file)?)?;
    crate::paths::restrict(&tmp, 0o600);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Every named key, without hashes.
pub fn list(paths: &Paths) -> Vec<KeyInfo> {
    load_file(paths).keys.into_iter().map(|k| k.info).collect()
}

/// Make a key named `name`; returns it with the secret, which is never shown again.
pub fn create(paths: &Paths, name: &str) -> Result<(KeyInfo, String)> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(|c| c.is_control()) {
        bail!("a key needs a name of 1 to 64 characters");
    }
    let mut file = load_file(paths);
    if file.keys.len() >= 100 {
        bail!("this server already has 100 keys; remove some with cove keys rm");
    }
    let secret = format!("cove_{}", crate::random_hex(24));
    let info = KeyInfo {
        id: format!("key_{}", crate::random_hex(6)),
        name: name.to_string(),
        created_at: crate::now(),
    };
    file.keys.push(StoredKey {
        info: info.clone(),
        hash: hash(&secret),
    });
    save_file(paths, &file)?;
    Ok((info, secret))
}

pub fn remove(paths: &Paths, id: &str) -> Result<KeyInfo> {
    let mut file = load_file(paths);
    let at = file
        .keys
        .iter()
        .position(|k| k.info.id == id || k.info.name == id)
        .ok_or_else(|| anyhow!("no key {id:?}"))?;
    let removed = file.keys.remove(at);
    save_file(paths, &file)?;
    Ok(removed.info)
}

/// Whether `given` is one of the named keys.
pub fn valid(paths: &Paths, given: &str) -> bool {
    if given.is_empty() {
        return false;
    }
    let h = hash(given);
    load_file(paths)
        .keys
        .iter()
        .any(|k| constant_time_eq(k.hash.as_bytes(), h.as_bytes()))
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Letters and digits that can't be mistaken for each other when typed.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// Wrong codes allowed in a row before pairing locks for a while.
const MAX_FAILURES: u32 = 10;
const LOCKOUT: Duration = Duration::from_secs(10 * 60);

struct Code {
    code: String,
    name: String,
    expires: Instant,
}

/// One-time pairing codes, held in the server's memory only.
#[derive(Default)]
pub struct Pairing {
    codes: Vec<Code>,
    failures: u32,
    locked_until: Option<Instant>,
}

impl Pairing {
    /// A new code; the key it gets will be called `name` unless the client names it.
    pub fn issue(&mut self, name: &str) -> (String, u64) {
        let now = Instant::now();
        self.codes.retain(|c| c.expires > now);
        let mut raw = [0u8; 8];
        getrandom::getrandom(&mut raw).expect("the OS random source is available");
        let chars: String = raw
            .iter()
            .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
            .collect();
        let code = format!("{}-{}", &chars[..4], &chars[4..]);
        self.codes.push(Code {
            code: code.clone(),
            name: name.to_string(),
            expires: now + CODE_TTL,
        });
        (code, crate::now() + CODE_TTL.as_secs())
    }

    /// Use a code: on success it is gone and the key it pairs gets this name.
    pub fn redeem(&mut self, given: &str) -> Result<String> {
        let now = Instant::now();
        if self.locked_until.is_some_and(|t| t > now) {
            bail!("too many wrong pairing codes; try again in a few minutes");
        }
        self.codes.retain(|c| c.expires > now);
        let given = normalize_code(given);
        match self
            .codes
            .iter()
            .position(|c| constant_time_eq(normalize_code(&c.code).as_bytes(), given.as_bytes()))
        {
            Some(at) => {
                self.failures = 0;
                Ok(self.codes.remove(at).name)
            }
            None => {
                self.failures += 1;
                if self.failures >= MAX_FAILURES {
                    self.failures = 0;
                    self.locked_until = Some(now + LOCKOUT);
                }
                bail!("that pairing code is wrong or has expired; make a new one with cove pair")
            }
        }
    }
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_hashed_listed_and_revoked() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path());
        let (info, secret) = create(&paths, "runo on my mac").unwrap();
        assert!(secret.starts_with("cove_"));
        assert!(valid(&paths, &secret));
        assert!(!valid(&paths, "cove_wrong"));
        assert!(!valid(&paths, ""));
        let text = std::fs::read_to_string(paths.keys_file()).unwrap();
        assert!(!text.contains(&secret), "the key itself is stored");
        assert!(text.contains(&hash(&secret)), "the hash is stored");
        let listed = list(&paths);
        assert_eq!(listed[0].name, "runo on my mac");
        // Clients never see the hash.
        assert!(!serde_json::to_string(&listed)
            .unwrap()
            .contains(&hash(&secret)));
        remove(&paths, &info.id).unwrap();
        assert!(!valid(&paths, &secret));
        assert!(create(&paths, "").is_err());
    }

    #[test]
    fn codes_work_once_in_any_case() {
        let mut p = Pairing::default();
        let (code, _) = p.issue("runo");
        assert_eq!(code.len(), 9);
        assert_eq!(
            p.redeem(&code.to_lowercase().replace('-', "")).unwrap(),
            "runo"
        );
        assert!(p.redeem(&code).is_err(), "a code works only once");
    }

    #[test]
    fn wrong_codes_lock_pairing() {
        let mut p = Pairing::default();
        let (code, _) = p.issue("runo");
        for _ in 0..MAX_FAILURES {
            assert!(p.redeem("AAAA-AAAA").is_err());
        }
        let refused = p.redeem(&code).unwrap_err();
        assert!(refused.to_string().contains("too many"), "{refused}");
    }
}
