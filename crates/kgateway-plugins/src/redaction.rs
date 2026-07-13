//! Redaction of captured request/response bodies (M11). Detects secrets/PII by regex,
//! replaces each hit with a stable placeholder `⟦REDACTED:n⟧`, and — when an encryption
//! key is configured — keeps a **reversible**, AES-256-GCM-encrypted mapping so an
//! operator with `logs:reveal` can un-redact. With no key, it still masks but drops the
//! mapping (reveal unavailable). See `docs/13-redaction-rbac-plan.md`.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use rand::RngCore;
use regex::{Regex, RegexSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Built-in secret/PII patterns. Ordered most-specific-ish first; overlaps are resolved
/// by earliest-start-wins in `redact`.
const BUILTIN_PATTERNS: &[&str] = &[
    // JWT (header.payload.signature)
    r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
    // OpenAI / Anthropic style API keys: sk-... / sk-ant-...
    r"sk-[A-Za-z0-9-]{20,}",
    // AWS access key id
    r"AKIA[0-9A-Z]{16}",
    // Bearer token in header-ish text
    r"(?i)bearer\s+[A-Za-z0-9._~+/-]+=*",
    // Email address
    r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
    // Credit-card-ish 13–16 digit run (optionally space/dash separated)
    r"\b(?:\d[ -]?){13,16}\b",
];

/// The 12-byte GCM nonce is prepended to the ciphertext before base64 encoding.
const NONCE_LEN: usize = 12;

/// The decrypted mapping: a per-record random `marker` plus the ordered original values.
/// The marker makes placeholders unforgeable — see [`Redactor::redact`].
#[derive(Serialize, Deserialize)]
struct Mapping {
    /// Random per-record hex marker embedded in every placeholder of this record.
    m: String,
    /// Original (pre-redaction) values, indexed by placeholder number.
    o: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RedactionError {
    #[error("invalid redaction pattern: {0}")]
    Pattern(String),
    #[error("no encryption key configured; reveal unavailable")]
    NoKey,
    #[error("mapping decode/decrypt failed")]
    Decrypt,
}

/// Outcome of redacting one body.
pub struct RedactionResult {
    /// The body with secrets replaced by `⟦REDACTED:n⟧` placeholders.
    pub redacted: String,
    /// Encrypted, base64 (placeholder→original) mapping. `None` when nothing was redacted
    /// OR no encryption key is configured (mask-only mode).
    pub mapping: Option<String>,
    /// Whether any span was redacted.
    pub any_redacted: bool,
}

/// Prefilter + per-pattern matchers + optional cipher. Cheap to share (`Arc`) across
/// requests.
pub struct Redactor {
    /// `RegexSet` over all patterns — a single-pass "does ANY pattern match?" prefilter.
    /// Most bodies contain no secrets, so this cheaply skips the per-pattern span scan and
    /// all downstream work for the common case.
    prefilter: RegexSet,
    /// Per-pattern matchers for span extraction (only run when the prefilter hits). Kept
    /// separate rather than one alternation regex: a big alternation of the complex
    /// patterns (bounded-repetition card numbers, `\b` boundaries) hit a slow match-
    /// extraction path — measurably slower on matching bodies than N fast-failing scans.
    patterns: Vec<Regex>,
    cipher: Option<Aes256Gcm>,
}

impl Redactor {
    /// Build from custom patterns (in addition to the built-ins) and an optional key
    /// passphrase (any length; hashed to 32 bytes). `None` key ⇒ mask-only (no reveal).
    pub fn new(custom_patterns: &[String], key: Option<&str>) -> Result<Self, RedactionError> {
        let all: Vec<&str> = BUILTIN_PATTERNS
            .iter()
            .copied()
            .chain(custom_patterns.iter().map(String::as_str))
            .collect();
        let mut patterns = Vec::with_capacity(all.len());
        for p in &all {
            patterns.push(Regex::new(p).map_err(|e| RedactionError::Pattern(e.to_string()))?);
        }
        let prefilter = RegexSet::new(&all).map_err(|e| RedactionError::Pattern(e.to_string()))?;
        let cipher = key.map(|k| {
            // Derive a 32-byte key from the passphrase. This is key-from-passphrase, not
            // password storage — SHA-256 is appropriate here. `new_from_slice` avoids
            // coupling to aes-gcm's `generic-array` version.
            let digest = Sha256::digest(k.as_bytes());
            Aes256Gcm::new_from_slice(digest.as_slice()).expect("sha-256 digest is 32 bytes")
        });
        Ok(Self {
            prefilter,
            patterns,
            cipher,
        })
    }

    /// Whether reveal is possible (a key was configured).
    pub fn can_reveal(&self) -> bool {
        self.cipher.is_some()
    }

    /// Redact `text`: replace matched spans with placeholders and (if keyed) return the
    /// encrypted mapping.
    pub fn redact(&self, text: &str) -> RedactionResult {
        // Fast path: one prefilter pass rules out the common "no secrets" body cheaply,
        // before any per-pattern scanning / allocation / crypto.
        if !self.prefilter.is_match(text) {
            return RedactionResult {
                redacted: text.to_string(),
                mapping: None,
                any_redacted: false,
            };
        }

        // Something matched — collect spans from each pattern, then resolve overlaps
        // (earliest-start wins; longest at a tie).
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for re in &self.patterns {
            for m in re.find_iter(text) {
                if m.end() > m.start() {
                    spans.push((m.start(), m.end()));
                }
            }
        }
        if spans.is_empty() {
            return RedactionResult {
                redacted: text.to_string(),
                mapping: None,
                any_redacted: false,
            };
        }
        spans.sort_by_key(|s| (s.0, std::cmp::Reverse(s.1)));
        let mut kept: Vec<(usize, usize)> = Vec::new();
        for (s, e) in spans {
            if kept.last().is_some_and(|last| s < last.1) {
                continue;
            }
            kept.push((s, e));
        }

        // A per-record random marker embedded in every placeholder. Because it's generated
        // AFTER the (attacker-influenceable) input is known and is unguessable, a body can't
        // pre-plant a literal `⟦REDACTED:<marker>:i⟧` that reveal would wrongly substitute.
        let marker = format!("{:032x}", rand::random::<u128>());

        let mut out = String::with_capacity(text.len());
        let mut originals: Vec<String> = Vec::with_capacity(kept.len());
        let mut cursor = 0;
        for (i, (s, e)) in kept.iter().enumerate() {
            out.push_str(&text[cursor..*s]);
            out.push_str(&format!("⟦REDACTED:{marker}:{i}⟧"));
            originals.push(text[*s..*e].to_string());
            cursor = *e;
        }
        out.push_str(&text[cursor..]);

        let mapping = self
            .cipher
            .as_ref()
            .and_then(|c| encrypt_mapping(c, &marker, &originals));

        RedactionResult {
            redacted: out,
            mapping,
            any_redacted: true,
        }
    }

    /// Reverse a redaction using its encrypted mapping. Requires a configured key. Only the
    /// exact `⟦REDACTED:<marker>:i⟧` placeholders minted for this record are substituted, so
    /// literal placeholder-looking text in the body is left untouched.
    pub fn reveal(
        &self,
        redacted: &str,
        encrypted_mapping: &str,
    ) -> Result<String, RedactionError> {
        let cipher = self.cipher.as_ref().ok_or(RedactionError::NoKey)?;
        let mapping = decrypt_mapping(cipher, encrypted_mapping)?;
        let mut out = redacted.to_string();
        for (i, orig) in mapping.o.iter().enumerate() {
            out = out.replace(&format!("⟦REDACTED:{}:{i}⟧", mapping.m), orig);
        }
        Ok(out)
    }
}

/// Encrypt the marker + ordered originals into a base64 `nonce ++ ciphertext` blob.
fn encrypt_mapping(cipher: &Aes256Gcm, marker: &str, originals: &[String]) -> Option<String> {
    let mapping = Mapping {
        m: marker.to_string(),
        o: originals.to_vec(),
    };
    let plaintext = serde_json::to_vec(&mapping).ok()?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext.as_ref()).ok()?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Some(base64::engine::general_purpose::STANDARD.encode(blob))
}

/// Reverse of [`encrypt_mapping`].
fn decrypt_mapping(cipher: &Aes256Gcm, blob_b64: &str) -> Result<Mapping, RedactionError> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(blob_b64.trim())
        .map_err(|_| RedactionError::Decrypt)?;
    if blob.len() <= NONCE_LEN {
        return Err(RedactionError::Decrypt);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| RedactionError::Decrypt)?;
    serde_json::from_slice(&plaintext).map_err(|_| RedactionError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_builtin_patterns() {
        let r = Redactor::new(&[], None).unwrap();
        let out = r.redact("email me at alice@example.com or use sk-abcdefghijklmnopqrstuvwxyz");
        assert!(out.any_redacted);
        assert!(!out.redacted.contains("alice@example.com"));
        assert!(!out.redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(out.redacted.contains("⟦REDACTED:"));
        // No key → no mapping (mask-only).
        assert!(out.mapping.is_none());
        assert!(!r.can_reveal());
    }

    #[test]
    fn reversible_roundtrip_with_key() {
        let r = Redactor::new(&[], Some("super-secret-passphrase")).unwrap();
        let original = "contact bob@corp.io and token eyJhbGc.eyJzdWI.sig here";
        let out = r.redact(original);
        assert!(out.any_redacted);
        assert!(!out.redacted.contains("bob@corp.io"));
        let mapping = out.mapping.expect("keyed → mapping present");

        let revealed = r.reveal(&out.redacted, &mapping).unwrap();
        assert_eq!(revealed, original);
    }

    #[test]
    fn nothing_to_redact_yields_no_mapping() {
        let r = Redactor::new(&[], Some("k")).unwrap();
        let out = r.redact("just a normal sentence with no secrets");
        assert!(!out.any_redacted);
        assert!(out.mapping.is_none());
        assert_eq!(out.redacted, "just a normal sentence with no secrets");
    }

    #[test]
    fn custom_pattern_is_applied() {
        let r = Redactor::new(&[r"SECRET-\d+".to_string()], Some("k")).unwrap();
        let out = r.redact("code SECRET-12345 end");
        assert!(!out.redacted.contains("SECRET-12345"));
        assert_eq!(
            r.reveal(&out.redacted, &out.mapping.unwrap()).unwrap(),
            "code SECRET-12345 end"
        );
    }

    #[test]
    fn reveal_without_key_errors() {
        let r = Redactor::new(&[], None).unwrap();
        assert!(matches!(r.reveal("x", "y"), Err(RedactionError::NoKey)));
    }

    #[test]
    fn tampered_mapping_fails_cleanly() {
        let r = Redactor::new(&[], Some("k")).unwrap();
        let out = r.redact("mail a@b.com");
        let mut m = out.mapping.unwrap();
        m.push_str("garbage");
        assert!(matches!(
            r.reveal(&out.redacted, &m),
            Err(RedactionError::Decrypt)
        ));
    }

    #[test]
    fn invalid_custom_pattern_errors() {
        assert!(Redactor::new(&[r"(unclosed".to_string()], None).is_err());
    }

    #[test]
    fn reveal_ignores_attacker_planted_placeholder() {
        // A body that pre-plants a literal placeholder alongside a real secret. Reveal must
        // restore ONLY the genuinely-redacted secret and leave the planted literal intact
        // (the per-record marker makes the planted `⟦REDACTED:0⟧` a non-match).
        let r = Redactor::new(&[], Some("k")).unwrap();
        let input = "planted ⟦REDACTED:0⟧ and real sk-aaaaaaaaaaaaaaaaaaaaaa";
        let out = r.redact(input);
        assert!(out.any_redacted);
        let revealed = r.reveal(&out.redacted, &out.mapping.unwrap()).unwrap();
        assert!(
            revealed.contains("planted ⟦REDACTED:0⟧"),
            "planted literal must survive verbatim: {revealed}"
        );
        assert!(revealed.contains("sk-aaaaaaaaaaaaaaaaaaaaaa"));
        // The planted literal was NOT turned into the secret.
        assert_eq!(revealed.matches("sk-aaaaaaaaaaaaaaaaaaaaaa").count(), 1);
    }

    #[test]
    fn markers_are_unique_per_record() {
        let r = Redactor::new(&[], Some("k")).unwrap();
        let a = r.redact("mail a@b.com");
        let b = r.redact("mail a@b.com");
        // Same input, but different random markers → different placeholders / ciphertext.
        assert_ne!(a.redacted, b.redacted);
    }

    #[test]
    fn overlapping_matches_do_not_panic_and_cover() {
        // A bearer token containing an email-like substring: ensure spans don't corrupt.
        let r = Redactor::new(&[], Some("k")).unwrap();
        let out = r.redact("Authorization: Bearer abc.def@ghi and sk-aaaaaaaaaaaaaaaaaaaaaa");
        assert!(out.any_redacted);
        // Round-trips exactly regardless of overlap handling.
        let revealed = r.reveal(&out.redacted, &out.mapping.unwrap()).unwrap();
        assert_eq!(
            revealed,
            "Authorization: Bearer abc.def@ghi and sk-aaaaaaaaaaaaaaaaaaaaaa"
        );
    }
}
