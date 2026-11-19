use crate::security::secrets::SecretStore;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use reiclaw_config::schema::OtpConfig;
use ring::hmac;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const OTP_SECRET_FILE: &str = "otp-secret";
const OTP_DIGITS: u32 = 6;
const OTP_ISSUER: &str = "ReiClaw";

#[derive(Debug)]
pub struct OtpValidator {
    config: OtpConfig,
    secret: Vec<u8>,
    cached_codes: Mutex<HashMap<String, u64>>,
}

impl OtpValidator {
    /// Load an already-enrolled OTP secret without generating a new one.
    /// Fails closed when missing or corrupted.
    pub fn load_existing(
        config: &OtpConfig,
        zeroclaw_dir: &Path,
        store: &SecretStore,
    ) -> Result<Self> {
        let secret_path = secret_file_path(zeroclaw_dir);
        let encoded = match fs::read_to_string(&secret_path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "OTP secret not initialized at {}; run `rei estop otp-init` first",
                    secret_path.display()
                );
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to read OTP secret file {}",
                        secret_path.display().to_string()
                    )
                });
            }
        };
        let decrypted = store
            .decrypt_existing(encoded.trim())
            .context("Failed to decrypt OTP secret file")?;
        let secret = decode_base32_secret(&decrypted)?;
        Ok(Self {
            config: config.clone(),
            secret,
            cached_codes: Mutex::new(HashMap::new()),
        })
    }

    pub fn from_config(
        config: &OtpConfig,
        zeroclaw_dir: &Path,
        store: &SecretStore,
    ) -> Result<(Self, Option<String>)> {
        let secret_path = secret_file_path(zeroclaw_dir);
        let (secret, generated) = if secret_path.exists() {
            let encoded = fs::read_to_string(&secret_path).with_context(|| {
                format!(
                    "Failed to read OTP secret file {}",
                    secret_path.display().to_string()
                )
            })?;
            let decrypted = store
                .decrypt(encoded.trim())
                .context("Failed to decrypt OTP secret file")?;
            (decode_base32_secret(&decrypted)?, false)
        } else {
            let raw: [u8; 20] = rand::random();
            let encoded_secret = encode_base32_secret(&raw);
            let encrypted = store
                .encrypt(&encoded_secret)
                .context("Failed to encrypt OTP secret")?;
            write_secret_file(&secret_path, &encrypted)?;
            (raw.to_vec(), true)
        };

        let validator = Self {
            config: config.clone(),
            secret,
            cached_codes: Mutex::new(HashMap::new()),
        };
        let uri = if generated {
            Some(validator.otpauth_uri())
        } else {
            None
        };
        Ok((validator, uri))
    }

    pub fn validate(&self, code: &str) -> Result<bool> {
        self.validate_at(code, unix_timestamp_now())
    }

    fn validate_at(&self, code: &str, now_secs: u64) -> Result<bool> {
        let normalized = code.trim();
        if normalized.len() != OTP_DIGITS as usize
            || !normalized.chars().all(|ch| ch.is_ascii_digit())
        {
            return Ok(false);
        }

        {
            let mut cache = self.cached_codes.lock();
            cache.retain(|_, expiry| *expiry >= now_secs);
            if cache
                .get(normalized)
                .is_some_and(|expiry| *expiry >= now_secs)
            {
                return Ok(true);
            }
        }

        let step = self.config.token_ttl_secs.max(1);
        let counter = now_secs / step;
        let counters = [
            counter.saturating_sub(1),
            counter,
            counter.saturating_add(1),
        ];

        let is_valid = counters
            .iter()
            .map(|c| compute_totp_code(&self.secret, *c))
            .any(|candidate| candidate == normalized);

        if is_valid {
            let mut cache = self.cached_codes.lock();
            cache.insert(
                normalized.to_string(),
                now_secs.saturating_add(self.config.cache_valid_secs),
            );
        }

        Ok(is_valid)
    }

    pub fn otpauth_uri(&self) -> String {
        let secret = encode_base32_secret(&self.secret);
        let account = "reiclaw";
        format!(
            "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&period={period}",
            issuer = OTP_ISSUER,
            period = self.config.token_ttl_secs.max(1)
        )
    }

    #[cfg(test)]
    pub fn code_for_timestamp(&self, timestamp: u64) -> String {
        let counter = timestamp / self.config.token_ttl_secs.max(1);
        compute_totp_code(&self.secret, counter)
    }
}

pub fn secret_file_path(zeroclaw_dir: &Path) -> PathBuf {
    zeroclaw_dir.join(OTP_SECRET_FILE)
}

fn write_secret_file(path: &Path, value: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create directory {}",
                parent.display().to_string()
            )
        })?;
    }

    let temp_path = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&temp_path, value).with_context(|| {
        format!(
            "Failed to write temporary OTP secret {}",
            temp_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600));
    }

    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "Failed to atomically replace OTP secret file {}",
            path.display()
        )
    })?;
    Ok(())
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn compute_totp_code(secret: &[u8], counter: u64) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let counter_bytes = counter.to_be_bytes();
    let digest = hmac::sign(&key, &counter_bytes);
    let hash = digest.as_ref();

    let offset = (hash[19] & 0x0f) as usize;
    let binary = ((u32::from(hash[offset]) & 0x7f) << 24)
        | (u32::from(hash[offset + 1]) << 16)
        | (u32::from(hash[offset + 2]) << 8)
        | u32::from(hash[offset + 3]);

    let code = binary % 10_u32.pow(OTP_DIGITS);
    format!("{code:0>6}")
}

fn encode_base32_secret(input: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    if input.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    let mut buffer = 0u16;
    let mut bits_left = 0u8;

    for byte in input {
        buffer = (buffer << 8) | u16::from(*byte);
        bits_left += 8;

        while bits_left >= 5 {
            let index = ((buffer >> (bits_left - 5)) & 0x1f) as usize;
            result.push(ALPHABET[index] as char);
            bits_left -= 5;
        }
    }

    if bits_left > 0 {
        let index = ((buffer << (5 - bits_left)) & 0x1f) as usize;
        result.push(ALPHABET[index] as char);
    }

    result
}

fn decode_base32_secret(raw: &str) -> Result<Vec<u8>> {
    fn decode_char(ch: char) -> Option<u8> {
        match ch {
            'A'..='Z' => Some((ch as u8) - b'A'),
            '2'..='7' => Some((ch as u8) - b'2' + 26),
            _ => None,
        }
    }

    let mut cleaned = raw
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '\t' | '\n' | '\r' | '-'))
        .collect::<String>()
        .to_ascii_uppercase();
    while cleaned.ends_with('=') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        anyhow::bail!("OTP secret is empty");
    }
    if matches!(cleaned.len() % 8, 1 | 3 | 6) {
        anyhow::bail!("OTP secret has invalid base32 length");
    }

    let mut output = Vec::new();
    let mut buffer = 0u32;
    let mut bits_left = 0u8;

    for ch in cleaned.chars() {
        let value = decode_char(ch)
            .with_context(|| format!("OTP secret contains invalid base32 character '{ch}'"))?;
        buffer = (buffer << 5) | u32::from(value);
        bits_left += 5;

        if bits_left >= 8 {
            let byte = ((buffer >> (bits_left - 8)) & 0xff) as u8;
            output.push(byte);
            bits_left -= 8;
        }
    }

    if bits_left > 0 && (buffer & ((1u32 << bits_left) - 1)) != 0 {
        anyhow::bail!("OTP secret contains non-zero trailing padding bits");
    }
    if output.len() != 20 {
        anyhow::bail!(
            "OTP secret must decode to exactly 20 bytes (got {} bytes)",
            output.len()
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_config() -> OtpConfig {
        OtpConfig {
            enabled: true,
            token_ttl_secs: 30,
            cache_valid_secs: 120,
            ..OtpConfig::default()
        }
    }

    #[test]
    fn valid_totp_code_is_accepted() {
        let dir = tempdir().unwrap();
        let store = SecretStore::new(dir.path(), true);
        let (validator, _) = OtpValidator::from_config(&test_config(), dir.path(), &store).unwrap();

        let now = 1_700_000_000u64;
        let code = validator.code_for_timestamp(now);
        assert!(validator.validate_at(&code, now).unwrap());
    }

    #[test]
    fn expired_totp_code_is_rejected() {
        let dir = tempdir().unwrap();
        let store = SecretStore::new(dir.path(), true);
        let (validator, _) = OtpValidator::from_config(&test_config(), dir.path(), &store).unwrap();

        let stale = 1_700_000_000u64;
        let now = stale + 300;
        let code = validator.code_for_timestamp(stale);
        assert!(!validator.validate_at(&code, now).unwrap());
    }

    #[test]
    fn wrong_totp_code_is_rejected() {
        let dir = tempdir().unwrap();
        let store = SecretStore::new(dir.path(), true);
        let (validator, _) = OtpValidator::from_config(&test_config(), dir.path(), &store).unwrap();
        assert!(!validator.validate_at("123456", 1_700_000_000).unwrap());
    }

    #[test]
    fn secret_is_generated_and_reused() {
        let dir = tempdir().unwrap();
        let store = SecretStore::new(dir.path(), true);

        let (first, first_uri) =
            OtpValidator::from_config(&test_config(), dir.path(), &store).unwrap();
        assert!(first_uri.is_some());

        let secret_path = secret_file_path(dir.path());
        let stored = fs::read_to_string(&secret_path).unwrap();
        assert!(SecretStore::is_encrypted(stored.trim()));

        let (second, second_uri) =
            OtpValidator::from_config(&test_config(), dir.path(), &store).unwrap();
        assert!(second_uri.is_none());

        let ts = 1_700_000_000u64;
        assert_eq!(first.code_for_timestamp(ts), second.code_for_timestamp(ts));
    }

    #[test]
    fn load_existing_fails_closed_without_creating_secret() {
        let dir = tempdir().unwrap();
        let store = SecretStore::new(dir.path(), true);
        let err = OtpValidator::load_existing(&test_config(), dir.path(), &store)
            .expect_err("missing otp-secret must fail closed");
        assert!(err.to_string().contains("otp-init"));
        assert!(!secret_file_path(dir.path()).exists());
    }

    #[test]
    fn load_existing_with_missing_or_corrupt_secret_key_leaves_dir_untouched() {
        let dir = tempdir().unwrap();
        let secret_path = secret_file_path(dir.path());
        fs::write(&secret_path, "enc2:000102030405060708090a0b0c0d0e0f1011").unwrap();

        let store = SecretStore::new(dir.path(), true);
        let err = OtpValidator::load_existing(&test_config(), dir.path(), &store)
            .expect_err("missing .secret_key must fail");
        assert!(!err.to_string().is_empty());
        assert!(!dir.path().join(".secret_key").exists());

        fs::write(dir.path().join(".secret_key"), "deadbeef").unwrap();
        let short_err = OtpValidator::load_existing(&test_config(), dir.path(), &store)
            .expect_err("short .secret_key must fail");
        assert!(!short_err.to_string().is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join(".secret_key")).unwrap(),
            "deadbeef"
        );
    }

    #[test]
    fn decode_base32_rejects_truncated_secrets_and_nonzero_tail_bits() {
        // Invalid base32 lengths (1, 3, 6 mod 8)
        assert!(decode_base32_secret("A").is_err());
        assert!(decode_base32_secret("ABC").is_err());
        assert!(decode_base32_secret("ABCDEF").is_err());
        // Non-zero trailing padding bits in 2-char (10-bit -> 8-bit + 2 pad bits) base32
        assert!(decode_base32_secret("AB").is_err());
        // Valid 10-byte secret is rejected because 20 bytes are required
        assert!(decode_base32_secret("JBSWY3DPEBLW64TM").is_err());
        // Valid 20-byte RFC 6238 secret passes
        let decoded = decode_base32_secret("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        assert_eq!(decoded.len(), 20);
    }
}
