// Encrypted secret store — defense-in-depth for API keys and tokens.
//
// Secrets are encrypted using ChaCha20-Poly1305 AEAD with a random key stored
// in `~/.zeroclaw/.secret_key` with restrictive file permissions (0600). The
// config file stores only hex-encoded ciphertext, never plaintext keys.
//
// Each encryption generates a fresh random 12-byte nonce, prepended to the
// ciphertext. The Poly1305 authentication tag prevents tampering.
//
// This prevents:
//   - Plaintext exposure in config files
//   - Casual `grep` or `git log` leaks
//   - Accidental commit of raw API keys
//   - Known-plaintext attacks (unlike the previous XOR cipher)
//   - Ciphertext tampering (authenticated encryption)
//
// For sovereign users who prefer plaintext, `secrets.encrypt = false` disables this.
//
// Migration: values with the legacy `enc:` prefix (XOR cipher) are decrypted
// using the old algorithm for backward compatibility. New encryptions always
// produce `enc2:` (ChaCha20-Poly1305).

use anyhow::{Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, ChaCha20Poly1305, Key, Nonce};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Length of the random encryption key in bytes (256-bit, matches `ChaCha20`).
const KEY_LEN: usize = 32;

/// ChaCha20-Poly1305 nonce length in bytes.
const NONCE_LEN: usize = 12;

const TACHI_VAULT_TIMEOUT: Duration = Duration::from_secs(10);

const ONEPASSWORD_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Manages encrypted storage of secrets (API keys, tokens, etc.)
#[derive(Debug, Clone)]
pub struct SecretStore {
    /// Path to the key file (`~/.zeroclaw/.secret_key`)
    key_path: PathBuf,
    /// Whether encryption is enabled
    enabled: bool,
}

impl SecretStore {
    /// Create a new secret store rooted at the given directory.
    pub fn new(zeroclaw_dir: &Path, enabled: bool) -> Self {
        Self {
            key_path: zeroclaw_dir.join(".secret_key"),
            enabled,
        }
    }

    /// Encrypt a plaintext secret. Returns hex-encoded ciphertext prefixed with `enc2:`.
    /// Format: `enc2:<hex(nonce ‖ ciphertext ‖ tag)>` (12 + N + 16 bytes).
    /// If encryption is disabled, returns the plaintext as-is.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        if !self.enabled || plaintext.is_empty() {
            return Ok(plaintext.to_string());
        }

        let key_bytes = self.load_or_create_key()?;
        let key = Key::from_slice(&key_bytes);
        let cipher = ChaCha20Poly1305::new(key);

        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher.encrypt(&nonce, plaintext.as_bytes()).map_err(|e| {
            ::reiclaw_log::record!(
                ERROR,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "ChaCha20-Poly1305 encryption failed"
            );
            anyhow::Error::msg(format!("Encryption failed: {e}"))
        })?;

        // Prepend nonce to ciphertext for storage
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);

        Ok(format!("enc2:{}", hex_encode(&blob)))
    }

    /// Decrypt a secret.
    /// - `enc2:` prefix → ChaCha20-Poly1305 (current format)
    /// - `enc:` prefix → legacy XOR cipher (backward compatibility for migration)
    /// - `op://` prefix → resolved via 1Password CLI (`op read`)
    /// - `vault:` prefix → resolved via Tachi Vault (`tachi vault`)
    /// - No prefix → returned as-is (plaintext config)
    ///
    /// **Warning**: Legacy `enc:` values are insecure. Use `decrypt_and_migrate` to
    /// automatically upgrade them to the secure `enc2:` format.
    pub fn decrypt(&self, value: &str) -> Result<String> {
        self.decrypt_with_key_policy(value, true)
    }

    /// Decrypt a secret without creating `.secret_key` if it is missing.
    pub fn decrypt_existing(&self, value: &str) -> Result<String> {
        self.decrypt_with_key_policy(value, false)
    }

    fn decrypt_with_key_policy(&self, value: &str, create_if_missing: bool) -> Result<String> {
        if let Some(hex_str) = value.strip_prefix("enc2:") {
            self.decrypt_chacha20_with_key_policy(hex_str, create_if_missing)
        } else if let Some(hex_str) = value.strip_prefix("enc:") {
            self.decrypt_legacy_xor_with_key_policy(hex_str, create_if_missing)
        } else if is_onepassword_ref(value) {
            resolve_onepassword_ref(value)
        } else if is_tachi_vault_ref(value) {
            resolve_tachi_vault_ref(value)
        } else {
            Ok(value.to_string())
        }
    }

    /// Decrypt a secret and return a migrated `enc2:` value if the input used legacy `enc:` format.
    ///
    /// Returns `(plaintext, Some(new_enc2_value))` if migration occurred, or
    /// `(plaintext, None)` if no migration was needed.
    ///
    /// This allows callers to persist the upgraded value back to config.
    pub fn decrypt_and_migrate(&self, value: &str) -> Result<(String, Option<String>)> {
        if let Some(hex_str) = value.strip_prefix("enc2:") {
            // Already using secure format — no migration needed
            let plaintext = self.decrypt_chacha20(hex_str)?;
            Ok((plaintext, None))
        } else if let Some(hex_str) = value.strip_prefix("enc:") {
            // Legacy XOR cipher — decrypt and re-encrypt with ChaCha20-Poly1305
            ::reiclaw_log::record!(
                WARN,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                    .with_outcome(::reiclaw_log::EventOutcome::Unknown),
                "Decrypting legacy XOR-encrypted secret (enc: prefix). \
                 This format is insecure and will be removed in a future release. \
                 The secret will be automatically migrated to enc2: (ChaCha20-Poly1305)."
            );
            let plaintext = self.decrypt_legacy_xor(hex_str)?;
            let migrated = self.encrypt(&plaintext)?;
            Ok((plaintext, Some(migrated)))
        } else if is_external_secret_ref(value) {
            let plaintext = self.decrypt(value)?;
            Ok((plaintext, None))
        } else {
            // Plaintext — no migration needed
            Ok((value.to_string(), None))
        }
    }

    /// Check if a value uses the legacy `enc:` format that should be migrated.
    pub fn needs_migration(value: &str) -> bool {
        value.starts_with("enc:")
    }

    /// Decrypt using ChaCha20-Poly1305 (current secure format).
    fn decrypt_chacha20(&self, hex_str: &str) -> Result<String> {
        self.decrypt_chacha20_with_key_policy(hex_str, true)
    }

    fn decrypt_chacha20_with_key_policy(
        &self,
        hex_str: &str,
        create_if_missing: bool,
    ) -> Result<String> {
        let blob =
            hex_decode(hex_str).context("Failed to decode encrypted secret (corrupt hex)")?;
        anyhow::ensure!(
            blob.len() > NONCE_LEN,
            "Encrypted value too short (missing nonce)"
        );

        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let key_bytes = if create_if_missing {
            self.load_or_create_key()?
        } else {
            self.load_existing_key()?
        };
        anyhow::ensure!(
            key_bytes.len() == KEY_LEN,
            "Secret key has invalid length: expected {} bytes, got {}",
            KEY_LEN,
            key_bytes.len()
        );
        let key = Key::from_slice(&key_bytes);
        let cipher = ChaCha20Poly1305::new(key);

        let plaintext_bytes = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| {
                ::reiclaw_log::record!(ERROR, ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail).with_outcome(::reiclaw_log::EventOutcome::Failure).with_attrs(::serde_json::json!({"key_path": self.key_path.display().to_string(), "error": format!("{e}")})), "enc2: decryption failed. `.secret_key` is missing or does not match the key used to encrypt this value. \
                     Common cause: volume wipe, container migration, or backup-restore where `.secret_key` was not preserved alongside `config.toml`. \
                     Restore the original `.secret_key` from backup, or re-encrypt the affected secrets via `rei quickstart`.");
                anyhow::Error::msg(format!(
                    "enc2: decryption failed (wrong `.secret_key` or tampered ciphertext): {e}"
                ))
            })?;

        String::from_utf8(plaintext_bytes)
            .context("Decrypted secret is not valid UTF-8 — corrupt data")
    }

    /// Decrypt using legacy XOR cipher (insecure, for backward compatibility only).
    fn decrypt_legacy_xor(&self, hex_str: &str) -> Result<String> {
        self.decrypt_legacy_xor_with_key_policy(hex_str, true)
    }

    fn decrypt_legacy_xor_with_key_policy(
        &self,
        hex_str: &str,
        create_if_missing: bool,
    ) -> Result<String> {
        let ciphertext = hex_decode(hex_str)
            .context("Failed to decode legacy encrypted secret (corrupt hex)")?;
        let key = if create_if_missing {
            self.load_or_create_key()?
        } else {
            self.load_existing_key()?
        };
        let plaintext_bytes = xor_cipher(&ciphertext, &key);
        String::from_utf8(plaintext_bytes)
            .context("Decrypted legacy secret is not valid UTF-8 — wrong key or corrupt data")
    }

    /// Check if a value is already encrypted or externally resolved.
    pub fn is_encrypted(value: &str) -> bool {
        value.starts_with("enc2:") || value.starts_with("enc:") || is_external_secret_ref(value)
    }

    /// Check if a value is a 1Password external secret reference.
    pub fn is_onepassword_ref(value: &str) -> bool {
        is_onepassword_ref(value)
    }

    /// Check if a value is a Tachi Vault external secret reference.
    pub fn is_tachi_vault_ref(value: &str) -> bool {
        is_tachi_vault_ref(value)
    }

    /// Check if a value is resolved by an external credential owner.
    pub fn is_external_secret_ref(value: &str) -> bool {
        is_external_secret_ref(value)
    }

    /// Check if a value uses the secure `enc2:` format.
    pub fn is_secure_encrypted(value: &str) -> bool {
        value.starts_with("enc2:")
    }

    /// Load an existing encryption key from disk without creating one if missing.
    fn load_existing_key(&self) -> Result<Vec<u8>> {
        let hex_key =
            fs::read_to_string(&self.key_path).context("Failed to read secret key file")?;
        let key = hex_decode(hex_key.trim()).context("Secret key file is corrupt")?;
        anyhow::ensure!(
            key.len() == KEY_LEN,
            "Secret key file has invalid length: expected {} bytes, got {}",
            KEY_LEN,
            key.len()
        );
        Ok(key)
    }

    /// Load the encryption key from disk, or create one if it doesn't exist.
    fn load_or_create_key(&self) -> Result<Vec<u8>> {
        if self.key_path.exists() {
            self.load_existing_key()
        } else {
            let key = generate_random_key();
            if let Some(parent) = self.key_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&self.key_path, hex_encode(&key))
                .context("Failed to write secret key file")?;

            // Set restrictive permissions
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&self.key_path, fs::Permissions::from_mode(0o600))
                    .context("Failed to set key file permissions")?;
            }

            Ok(key)
        }
    }
}

/// XOR cipher with repeating key. Same function for encrypt and decrypt.
fn xor_cipher(data: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    data.iter()
        .enumerate()
        .map(|(i, &b)| b ^ key[i % key.len()])
        .collect()
}

/// Generate a random 256-bit key using the OS CSPRNG.
///
/// Uses `OsRng` (via `getrandom`) directly, providing full 256-bit entropy
/// without the fixed version/variant bits that UUID v4 introduces.
fn generate_random_key() -> Vec<u8> {
    ChaCha20Poly1305::generate_key(&mut OsRng).to_vec()
}

/// Hex-encode bytes to a lowercase hex string.
fn hex_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Hex-decode a hex string to bytes.
#[allow(clippy::manual_is_multiple_of)]
fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if (hex.len() & 1) != 0 {
        anyhow::bail!("Hex string has odd length");
    }
    // Reject non-ASCII up front: valid hex is always ASCII, and this guarantees
    // every byte is a char boundary so the byte-index slicing below cannot panic
    // on a corrupt/tampered ciphertext (it returns the Err the signature promises).
    if !hex.is_ascii() {
        anyhow::bail!("Hex string contains non-ASCII characters");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::Error::msg(format!("Invalid hex at position {i}: {e}")))
        })
        .collect()
}

fn is_onepassword_ref(value: &str) -> bool {
    value.starts_with("op://")
}

pub fn is_tachi_vault_ref(value: &str) -> bool {
    value.starts_with("vault:")
}

pub fn is_external_secret_ref(value: &str) -> bool {
    is_onepassword_ref(value) || is_tachi_vault_ref(value)
}

fn validate_tachi_vault_ref(reference: &str) -> Result<&str> {
    let name = reference.strip_prefix("vault:").unwrap_or("");
    let bytes = name.as_bytes();
    anyhow::ensure!(
        (1..=128).contains(&bytes.len())
            && matches!(bytes[0], b'A'..=b'Z' | b'a'..=b'z' | b'_')
            && bytes[1..]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        "Invalid Tachi Vault reference name; expected vault:[A-Za-z_][A-Za-z0-9_.-]* (1..=128 ASCII bytes)"
    );
    Ok(name)
}

const MAX_TACHI_VAULT_PIPE_BYTES: u64 = 64 * 1024;
const TACHI_VAULT_LEASE_ENV: &str = "REI_VAULT_SECRET";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TachiVaultStatus {
    Daemon {
        initialized: bool,
        locked: bool,
        auto_unlock: bool,
    },
    Offline {
        initialized: bool,
    },
}

#[derive(Debug)]
enum TachiVaultExecError {
    NonZero {
        error: anyhow::Error,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Other(anyhow::Error),
}

impl TachiVaultExecError {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::NonZero { error, .. } | Self::Other(error) => error,
        }
    }
}

enum TachiVaultLeaseError {
    /// Tachi explicitly refused leasing because the entry is a typed
    /// non-`api_key` secret (for example `token`, `generic`, or `config`).
    NotApiKey(anyhow::Error),
    /// Lease was refused by policy/health/ACL/lock/missing entry or failed
    /// with a protocol/transport/parse error. Never fall back to raw `vault get`.
    RefusedOrProtocol(anyhow::Error),
}

/// Run one non-interactive CLI step. Never attach stdout or raw stderr to an
/// error, even when decoding fails: either pipe can contain sensitive output.
fn run_tachi_vault_exec(
    args: &[&str],
    reference: &str,
    deadline: Instant,
) -> std::result::Result<Vec<u8>, TachiVaultExecError> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new("tachi")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            let error = if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::msg(
                    "Tachi CLI (`tachi`) not found. Install it to use vault: secret references in config.",
                )
            } else {
                anyhow::Error::msg(format!("Failed to run Tachi CLI: {e}"))
            };
            TachiVaultExecError::Other(error)
        })?;
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture Tachi CLI stdout")
        .map_err(TachiVaultExecError::Other)?;
    let stderr = child
        .stderr
        .take()
        .context("Failed to capture Tachi CLI stderr")
        .map_err(TachiVaultExecError::Other)?;
    // Drain both pipes concurrently with a fixed byte cap. Channel receives
    // share the caller deadline so inherited pipes cannot extend the timeout.
    let read_pipe = |pipe: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(MAX_TACHI_VAULT_PIPE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send(result);
        });
        rx
    };
    let stdout_rx = read_pipe(Box::new(stdout));
    let stderr_rx = read_pipe(Box::new(stderr));
    let output = (|| {
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .context("Failed to poll Tachi CLI process")
                .map_err(TachiVaultExecError::Other)?
            {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(TachiVaultExecError::Other(anyhow::Error::msg(format!(
                    "Tachi CLI timed out resolving \"{reference}\" after 10s"
                ))));
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let stdout = stdout_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Tachi CLI stdout reader timed out or disconnected")
            .map_err(TachiVaultExecError::Other)?
            .context("Failed to read Tachi CLI stdout")
            .map_err(TachiVaultExecError::Other)?;
        let stderr = stderr_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Tachi CLI stderr reader timed out or disconnected")
            .map_err(TachiVaultExecError::Other)?
            .context("Failed to read Tachi CLI stderr")
            .map_err(TachiVaultExecError::Other)?;
        if stdout.len() as u64 > MAX_TACHI_VAULT_PIPE_BYTES
            || stderr.len() as u64 > MAX_TACHI_VAULT_PIPE_BYTES
        {
            return Err(TachiVaultExecError::Other(anyhow::Error::msg(format!(
                "Tachi CLI output exceeded 64 KiB limit while resolving \"{reference}\""
            ))));
        }
        if !status.success() {
            let reason = classify_tachi_vault_stderr(&stderr);
            let exit_code = status.code();
            let error = match exit_code {
                Some(code) => anyhow::Error::msg(format!(
                    "Tachi CLI failed while resolving \"{reference}\" (exit status {code}): {reason}"
                )),
                None => anyhow::Error::msg(format!(
                    "Tachi CLI terminated by signal while resolving \"{reference}\""
                )),
            };
            return Err(TachiVaultExecError::NonZero {
                error,
                exit_code,
                stdout,
                stderr,
            });
        }
        Ok(stdout)
    })();
    if output.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    output
}

fn run_tachi_vault(args: &[&str], reference: &str, deadline: Instant) -> Result<Vec<u8>> {
    run_tachi_vault_exec(args, reference, deadline).map_err(TachiVaultExecError::into_error)
}

fn is_non_api_key_lease_refusal(name: &str, stderr: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stderr) else {
        return false;
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let error_line = match lines.as_slice() {
        [only] => *only,
        [first, second] if first.starts_with("tachi: logging to ") => *second,
        _ => return false,
    };
    let normalized = error_line.to_ascii_lowercase();
    let body = normalized
        .strip_prefix("fatal:")
        .or_else(|| normalized.strip_prefix("error:"))
        .map(str::trim_start)
        .unwrap_or(&normalized);
    let prefix = format!("vault name '{}' is ", name.to_ascii_lowercase());
    let Some(rest) = body.strip_prefix(&prefix) else {
        return false;
    };
    if matches!(
        rest,
        "lane config, not a credential; refusing secret_type=api_key"
            | "lane config, not a credential; refusing to lease it as an api key"
    ) {
        return true;
    }
    let Some(secret_type) =
        rest.strip_suffix(", not a credential; refusing to lease it as an api key")
    else {
        return false;
    };
    !secret_type.is_empty()
        && secret_type != "api_key"
        && secret_type
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn classify_tachi_vault_stderr(stderr: &[u8]) -> &'static str {
    let lower = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if lower.contains("no usable api key")
        || lower.contains("disabled")
        || lower.contains("auth-failed")
        || lower.contains("auth_failed")
        || lower.contains("rate-limited")
        || lower.contains("rate_limited")
        || lower.contains("cooling")
        || lower.contains("restricted")
        || lower.contains("access denied")
    {
        "no usable API key is eligible for lease"
    } else if lower.contains("not a credential; refusing to lease it as an api key")
        || lower.contains("not a credential; refusing secret_type=api_key")
    {
        "entry is not a leasable API key"
    } else if lower.contains("not found")
        || lower.contains("no secret")
        || lower.contains("missing")
        || lower.contains("does not exist")
    {
        "requested secret entry was not found in Tachi Vault"
    } else if lower.contains("keychain") {
        "Tachi Vault password is not available in macOS Keychain"
    } else if lower.contains("not initialized") {
        "Tachi Vault is not initialized"
    } else if lower.contains("locked") || lower.contains("unlock") || lower.contains("password") {
        "Tachi Vault is locked or authentication failed"
    } else if lower.contains("api_key") || lower.contains("pool") || lower.contains("lease") {
        "entry cannot be leased as an API key"
    } else {
        "command exited with non-zero status"
    }
}

fn parse_tachi_vault_status(raw: &[u8]) -> Result<TachiVaultStatus> {
    if raw.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{') {
        let status: serde_json::Value = serde_json::from_slice(raw)
            .map_err(|_| anyhow::Error::msg("Tachi Vault returned invalid status JSON"))?;
        let initialized = status
            .get("initialized")
            .and_then(serde_json::Value::as_bool)
            .context("Tachi Vault status missing boolean initialized")?;
        let locked = status
            .get("locked")
            .and_then(serde_json::Value::as_bool)
            .context("Tachi Vault status missing boolean locked")?;
        let auto_unlock = status
            .pointer("/secure_store/auto_unlock_available")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        return Ok(TachiVaultStatus::Daemon {
            initialized,
            locked,
            auto_unlock,
        });
    }

    let text = std::str::from_utf8(raw)
        .map_err(|_| anyhow::Error::msg("Tachi Vault returned unrecognized status output"))?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("Vault status:") {
        anyhow::bail!("Tachi Vault returned unrecognized status output");
    }
    for line in lines {
        if let Some(rest) = line.strip_prefix("initialized:") {
            return match rest.trim() {
                "true" => Ok(TachiVaultStatus::Offline { initialized: true }),
                "false" => Ok(TachiVaultStatus::Offline { initialized: false }),
                _ => Err(anyhow::Error::msg(
                    "Tachi Vault status missing boolean initialized",
                )),
            };
        }
    }
    Err(anyhow::Error::msg(
        "Tachi Vault status missing boolean initialized",
    ))
}

fn resolve_tachi_vault_lease(
    name: &str,
    reference: &str,
    auth_flag: &str,
    deadline: Instant,
) -> std::result::Result<String, TachiVaultLeaseError> {
    let output = run_tachi_vault_exec(
        &[
            "vault",
            "lease",
            name,
            "--env-name",
            TACHI_VAULT_LEASE_ENV,
            auth_flag,
            "--json",
        ],
        reference,
        deadline,
    )
    .map_err(|exec_err| match exec_err {
        TachiVaultExecError::NonZero {
            error,
            exit_code: Some(1),
            stdout,
            stderr,
        } if stdout.iter().all(u8::is_ascii_whitespace)
            && is_non_api_key_lease_refusal(name, &stderr) =>
        {
            TachiVaultLeaseError::NotApiKey(error)
        }
        other => TachiVaultLeaseError::RefusedOrProtocol(other.into_error()),
    })?;
    let output: serde_json::Value = serde_json::from_slice(&output).map_err(|_| {
        TachiVaultLeaseError::RefusedOrProtocol(anyhow::Error::msg(
            "Tachi Vault returned invalid lease JSON",
        ))
    })?;
    if output.get("leased").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(TachiVaultLeaseError::RefusedOrProtocol(anyhow::Error::msg(
            "Tachi Vault did not lease the requested secret",
        )));
    }
    let env_pointer = format!("/env/{TACHI_VAULT_LEASE_ENV}");
    let value = output
        .pointer(&env_pointer)
        .and_then(serde_json::Value::as_str)
        .context("Tachi Vault lease response missing string value")
        .map_err(TachiVaultLeaseError::RefusedOrProtocol)?;
    if value.is_empty() {
        return Err(TachiVaultLeaseError::RefusedOrProtocol(anyhow::Error::msg(
            "Tachi Vault returned an empty secret value",
        )));
    }
    Ok(value.to_string())
}

fn resolve_tachi_vault_get_keychain(
    name: &str,
    reference: &str,
    deadline: Instant,
) -> Result<String> {
    // Always pass `--keychain` so `tachi vault get` never falls back to
    // interactive `/dev/tty` password prompting.
    let output = run_tachi_vault(
        &["vault", "get", name, "--reveal", "--json", "--keychain"],
        reference,
        deadline,
    )?;
    let output: serde_json::Value = serde_json::from_slice(&output)
        .map_err(|_| anyhow::Error::msg("Tachi Vault returned invalid secret JSON"))?;
    anyhow::ensure!(
        output.get("revealed").and_then(serde_json::Value::as_bool) == Some(true),
        "Tachi Vault did not reveal the requested secret"
    );
    let value = output
        .get("value")
        .and_then(serde_json::Value::as_str)
        .context("Tachi Vault secret response missing string value")?;
    anyhow::ensure!(
        !value.is_empty(),
        "Tachi Vault returned an empty secret value"
    );
    Ok(value.to_string())
}

fn ensure_fallback_status_allowed(
    reference: &str,
    deadline: Instant,
    allow_offline: bool,
) -> Result<()> {
    let status_raw = run_tachi_vault(&["vault", "status"], reference, deadline)?;
    match parse_tachi_vault_status(&status_raw)? {
        TachiVaultStatus::Daemon {
            initialized: true,
            locked: false,
            auto_unlock: true,
        } => Ok(()),
        TachiVaultStatus::Offline { initialized: true } if allow_offline => Ok(()),
        TachiVaultStatus::Daemon {
            initialized: false, ..
        }
        | TachiVaultStatus::Offline { initialized: false } => Err(anyhow::Error::msg(format!(
            "Tachi Vault is not initialized while resolving \"{reference}\" (hint: run `tachi vault init` first)"
        ))),
        TachiVaultStatus::Daemon { locked: true, .. } => Err(anyhow::Error::msg(format!(
            "Tachi Vault daemon is locked while resolving \"{reference}\"; refusing offline Keychain bypass (hint: run `tachi vault unlock` first)"
        ))),
        TachiVaultStatus::Daemon {
            locked: false,
            auto_unlock: false,
            ..
        } => Err(anyhow::Error::msg(format!(
            "Tachi Vault entry \"{reference}\" is not a leasable API key and automatic Keychain unlock is unavailable for direct store read (hint: store the vault master password in macOS Keychain under service `tachi-vault` / account `default` for non-api_key secrets)"
        ))),
        TachiVaultStatus::Offline { initialized: true } => Err(anyhow::Error::msg(format!(
            "Tachi Vault daemon state changed while resolving \"{reference}\"; refusing offline Keychain bypass"
        ))),
    }
}

fn resolve_tachi_vault_ref(reference: &str) -> Result<String> {
    let name = validate_tachi_vault_ref(reference)?;
    let deadline = Instant::now() + TACHI_VAULT_TIMEOUT;
    let status_raw = run_tachi_vault(&["vault", "status"], reference, deadline)?;
    let status = parse_tachi_vault_status(&status_raw)?;
    let initialized = match status {
        TachiVaultStatus::Daemon { initialized, .. }
        | TachiVaultStatus::Offline { initialized } => initialized,
    };
    anyhow::ensure!(
        initialized,
        "Tachi Vault is not initialized while resolving \"{reference}\" (hint: run `tachi vault init` first)"
    );
    match status {
        TachiVaultStatus::Daemon {
            locked: false,
            auto_unlock,
            ..
        } => match resolve_tachi_vault_lease(name, reference, "--stdin-password", deadline) {
            Ok(secret) => Ok(secret),
            Err(TachiVaultLeaseError::NotApiKey(_)) if auto_unlock => {
                // Re-verify the daemon stayed unlocked across direct Keychain
                // store fallback because Tachi validates non-api_key types
                // before contacting the daemon.
                ensure_fallback_status_allowed(reference, deadline, false)?;
                let secret = resolve_tachi_vault_get_keychain(name, reference, deadline)?;
                ensure_fallback_status_allowed(reference, deadline, false)?;
                Ok(secret)
            }
            Err(TachiVaultLeaseError::NotApiKey(lease_err)) => Err(lease_err.context(format!(
                "Tachi Vault entry \"{reference}\" is not a leasable API key and automatic Keychain unlock is unavailable for direct store read (hint: store the vault master password in macOS Keychain under service `tachi-vault` / account `default` for non-api_key secrets)"
            ))),
            Err(TachiVaultLeaseError::RefusedOrProtocol(lease_err)) => Err(lease_err),
        },
        TachiVaultStatus::Daemon { locked: true, .. } => Err(anyhow::Error::msg(format!(
            "Tachi Vault daemon is locked while resolving \"{reference}\"; refusing offline Keychain bypass (hint: run `tachi vault unlock` first)"
        ))),
        TachiVaultStatus::Offline { .. } => {
            match resolve_tachi_vault_lease(name, reference, "--keychain", deadline) {
                Ok(secret) => Ok(secret),
                Err(TachiVaultLeaseError::NotApiKey(_)) => {
                    // Re-verify no locked daemon started before or during direct Keychain read.
                    ensure_fallback_status_allowed(reference, deadline, true)?;
                    let secret = resolve_tachi_vault_get_keychain(name, reference, deadline)?;
                    ensure_fallback_status_allowed(reference, deadline, true)?;
                    Ok(secret)
                }
                Err(TachiVaultLeaseError::RefusedOrProtocol(lease_err)) => Err(lease_err),
            }
        }
    }
}

fn validate_onepassword_ref(reference: &str) -> Result<()> {
    let path = reference.strip_prefix("op://").unwrap_or("");
    let mut segments = path.split('/');
    let has_required_segments = (0..3).all(|_| segments.next().is_some_and(|s| !s.is_empty()));
    anyhow::ensure!(
        has_required_segments && segments.all(|segment| !segment.is_empty()),
        "Invalid 1Password reference \"{reference}\". Expected format: op://vault-name/item-name/field-name"
    );
    Ok(())
}

/// Resolve a 1Password secret reference by invoking the `op` CLI.
fn resolve_onepassword_ref(reference: &str) -> Result<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    validate_onepassword_ref(reference)?;

    let mut child = Command::new("op")
        .args(["read", reference])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ::reiclaw_log::record!(
                ERROR,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                    .with_outcome(::reiclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": e.to_string()})),
                "Failed to run 1Password CLI"
            );
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::msg(
                    "1Password CLI (`op`) not found. Install it to use op:// secret references in config."
                )
            } else {
                anyhow::Error::msg(format!("Failed to run 1Password CLI: {e}"))
            }
        })?;

    let mut stdout = child
        .stdout
        .take()
        .context("Failed to capture 1Password CLI stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Failed to capture 1Password CLI stderr")?;
    let stdout_handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).map(|_| output)
    });

    let deadline = Instant::now() + ONEPASSWORD_READ_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("Failed to poll 1Password CLI process")?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            anyhow::bail!(
                "1Password CLI timed out resolving \"{reference}\" after {}s",
                ONEPASSWORD_READ_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow::Error::msg("1Password CLI stdout reader panicked"))?
        .context("Failed to read 1Password CLI stdout")?;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::Error::msg("1Password CLI stderr reader panicked"))?
        .context("Failed to read 1Password CLI stderr")?;

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr);
        let hint =
            if stderr_text.contains("not signed in") || stderr_text.contains("session expired") {
                " (hint: run `op signin` first)"
            } else {
                ""
            };
        anyhow::bail!(
            "1Password CLI failed to resolve \"{reference}\": {}{hint}",
            stderr_text.trim()
        );
    }

    let secret = String::from_utf8(stdout)
        .context("1Password CLI returned non-UTF-8 output")?
        .trim_end_matches(&['\r', '\n'][..])
        .to_string();

    anyhow::ensure!(
        !secret.is_empty(),
        "1Password CLI returned empty value for \"{reference}\". Verify the vault/item/field path is correct."
    );

    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::ffi::OsString;
    use tempfile::TempDir;

    #[cfg(unix)]
    struct EnvValueGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    #[cfg(unix)]
    impl EnvValueGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: tests that mutate env vars serialize on env_test_lock().
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    #[cfg(unix)]
    impl Drop for EnvValueGuard {
        fn drop(&mut self) {
            // SAFETY: tests that mutate env vars serialize on env_test_lock().
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[cfg(unix)]
    fn write_fake_op(bin_dir: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;

        let op_path = bin_dir.join("op");
        fs::write(&op_path, script).expect("write fake op");
        let mut perms = fs::metadata(&op_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&op_path, perms).unwrap();
    }

    #[test]
    fn tachi_vault_reference_classification_and_validation() {
        for reference in [
            "vault:OPENAI_OFFICIAL_API_KEY",
            "vault:openai-prod.v1",
            "vault:_a",
            &format!("vault:{}", "A".repeat(128)),
        ] {
            assert!(is_tachi_vault_ref(reference));
            assert!(SecretStore::is_tachi_vault_ref(reference));
            assert!(is_external_secret_ref(reference));
            assert!(SecretStore::is_external_secret_ref(reference));
            assert!(SecretStore::is_encrypted(reference));
            assert!(!SecretStore::needs_migration(reference));
            assert!(!SecretStore::is_secure_encrypted(reference));
            assert!(validate_tachi_vault_ref(reference).is_ok());
        }
        assert!(SecretStore::is_external_secret_ref("op://vault/item/field"));
        for value in ["plain", "enc2:abc", "VAULT:key"] {
            assert!(!SecretStore::is_tachi_vault_ref(value));
            assert!(!SecretStore::is_external_secret_ref(value));
        }
        for reference in [
            "vault:",
            "vault:-flag",
            "vault:foo/bar",
            "vault:foo bar",
            "vault:foo:bar",
            "vault:0key",
            "vault:é",
            "vault:foo\n",
            "not-vault:A",
            &format!("vault:{}", "A".repeat(129)),
        ] {
            assert!(
                validate_tachi_vault_ref(reference).is_err(),
                "{reference:?}"
            );
        }
    }

    #[cfg(unix)]
    fn write_fake_tachi_script(bin_dir: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = bin_dir.join("tachi");
        fs::write(&path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn write_fake_tachi_with_stderr(
        bin_dir: &Path,
        status: &str,
        lease_out: &str,
        lease_stderr: &str,
        lease_exit: u8,
        get_out: &str,
        get_exit: u8,
    ) {
        let sh_escape = |s: &str| s.replace('\'', "'\\''");
        let status = sh_escape(status);
        let lease_out = sh_escape(lease_out);
        let lease_stderr = sh_escape(lease_stderr);
        let get_out = sh_escape(get_out);
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$TACHI_TEST_CALLS"
printf '%s\n' 'tachi: logging to synthetic-private-log-path' >&2
if [ "$*" = 'vault status' ]; then
  printf '%s\n' '{status}'
  exit 0
fi
case "$*" in
  'vault lease '* )
    case "$*" in
      *' --stdin-password'* | *' --keychain'* ) ;;
      * ) printf '%s\n' 'interactive lease forbidden' >&2; exit 99 ;;
    esac
    printf '%s\n' '{lease_out}'
    if [ {lease_exit} -ne 0 ]; then printf '%s\n' '{lease_stderr}' >&2; fi
    exit {lease_exit}
    ;;
  'vault get '* )
    case "$*" in
      *' --keychain'* ) ;;
      * ) printf '%s\n' 'interactive get forbidden' >&2; exit 99 ;;
    esac
    printf '%s\n' '{get_out}'
    if [ {get_exit} -ne 0 ]; then printf '%s\n' 'entry missing: synthetic-stderr-secret' >&2; fi
    exit {get_exit}
    ;;
esac
printf '%s\n' 'unexpected subcommand' >&2
exit 98
"#
        );
        write_fake_tachi_script(bin_dir, &script);
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn write_fake_tachi(
        bin_dir: &Path,
        status: &str,
        lease_out: &str,
        lease_exit: u8,
        get_out: &str,
        get_exit: u8,
    ) {
        write_fake_tachi_with_stderr(
            bin_dir,
            status,
            lease_out,
            "lease refused: synthetic-stderr-secret",
            lease_exit,
            get_out,
            get_exit,
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tachi_vault_unlocked_and_keychain_resolution() {
        let _guard = crate::env_overrides::env_test_lock().await;
        let tmp = TempDir::new().unwrap();
        let _path = EnvValueGuard::set("PATH", tmp.path());
        let calls = tmp.path().join("calls");
        let _calls = EnvValueGuard::set("TACHI_TEST_CALLS", &calls);
        let store = SecretStore::new(tmp.path(), true);

        // 1. Unlocked daemon WITHOUT Keychain resolves api_key via non-interactive `vault lease`,
        // preserving exact bytes (including trailing CR/LF) encoded inside JSON.
        fs::write(&calls, "").unwrap();
        write_fake_tachi(
            tmp.path(),
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":false}}"#,
            r#"{"leased":true,"env":{"REI_VAULT_SECRET":"synthetic-leased-secret\r\n"}}"#,
            0,
            "",
            1,
        );
        let (secret, migrated) = store
            .decrypt_and_migrate("vault:OPENAI_OFFICIAL_API_KEY")
            .unwrap();
        assert_eq!(secret, "synthetic-leased-secret\r\n");
        assert!(migrated.is_none());
        assert_eq!(
            fs::read_to_string(&calls).unwrap(),
            "vault status\nvault lease OPENAI_OFFICIAL_API_KEY --env-name REI_VAULT_SECRET --stdin-password --json\n"
        );

        // 2. Unlocked daemon WITH Keychain falls back to `vault get --keychain` ONLY when
        // `vault lease` explicitly identifies the target as a non-api_key secret.
        // Outer CLI framing newlines after JSON are ignored while inner `\r\n` bytes stay intact.
        fs::write(&calls, "").unwrap();
        write_fake_tachi_with_stderr(
            tmp.path(),
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#,
            "",
            "Error: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key",
            1,
            r#"{"name":"BOT_TOKEN","revealed":true,"value":"synthetic-token\r\n"}"#,
            0,
        );
        assert_eq!(
            store.decrypt("vault:BOT_TOKEN").unwrap(),
            "synthetic-token\r\n"
        );
        assert_eq!(
            fs::read_to_string(&calls).unwrap(),
            "vault status\nvault lease BOT_TOKEN --env-name REI_VAULT_SECRET --stdin-password --json\nvault status\nvault get BOT_TOKEN --reveal --json --keychain\nvault status\n"
        );

        // 2b. Real `tachi` Fatal: prefix and `lane config` refusal formats also fall back cleanly.
        for fatal_stderr in [
            "Fatal: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key",
            "Fatal: Vault name 'BOT_TOKEN' is lane config, not a credential; refusing to lease it as an API key",
        ] {
            fs::write(&calls, "").unwrap();
            write_fake_tachi_with_stderr(
                tmp.path(),
                r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#,
                "",
                fatal_stderr,
                1,
                r#"{"name":"BOT_TOKEN","revealed":true,"value":"synthetic-fatal-token"}"#,
                0,
            );
            assert_eq!(
                store.decrypt("vault:BOT_TOKEN").unwrap(),
                "synthetic-fatal-token"
            );
        }

        // 3. Non-api_key secret whose name contains policy words (e.g. LOCKED_BOT_TOKEN)
        // still falls back when Tachi explicitly reports a non-api_key secret type.
        fs::write(&calls, "").unwrap();
        write_fake_tachi_with_stderr(
            tmp.path(),
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#,
            "",
            "Error: Vault name 'LOCKED_BOT_TOKEN' is generic, not a credential; refusing to lease it as an API key",
            1,
            r#"{"name":"LOCKED_BOT_TOKEN","revealed":true,"value":"synthetic-named-token"}"#,
            0,
        );
        assert_eq!(
            store.decrypt("vault:LOCKED_BOT_TOKEN").unwrap(),
            "synthetic-named-token"
        );

        // 4. Secrets consisting solely of CR/LF bytes are preserved verbatim rather than treated as empty.
        write_fake_tachi_with_stderr(
            tmp.path(),
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#,
            "",
            "Error: Vault name 'CRLF_ONLY' is generic, not a credential; refusing to lease it as an API key",
            1,
            r#"{"name":"CRLF_ONLY","revealed":true,"value":"\r\n"}"#,
            0,
        );
        assert_eq!(store.decrypt("vault:CRLF_ONLY").unwrap(), "\r\n");

        // 5. Offline plain-text status (no daemon running) uses Keychain-backed `vault lease --keychain`
        // for API keys / pools / slots so key health and rotation rules stay enforced offline.
        fs::write(&calls, "").unwrap();
        write_fake_tachi(
            tmp.path(),
            "Vault status:\n  initialized: true\n  entries: 3",
            "{\"leased\":true,\"env\":{\"REI_VAULT_SECRET\":\"synthetic-offline-leased\"}}\r",
            0,
            "",
            1,
        );
        assert_eq!(
            store.decrypt("vault:OPENAI_OFFICIAL_API_KEY").unwrap(),
            "synthetic-offline-leased"
        );
        assert_eq!(
            fs::read_to_string(&calls).unwrap(),
            "vault status\nvault lease OPENAI_OFFICIAL_API_KEY --env-name REI_VAULT_SECRET --keychain --json\n"
        );

        // 6. Offline mode falls back to `vault get --keychain` ONLY on explicit non-api_key refusal.
        fs::write(&calls, "").unwrap();
        write_fake_tachi_with_stderr(
            tmp.path(),
            "Vault status:\n  initialized: true\n  entries: 3",
            "",
            "Error: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key",
            1,
            "{\"name\":\"BOT_TOKEN\",\"revealed\":true,\"value\":\"synthetic-offline-token\"}\r",
            0,
        );
        assert_eq!(
            store.decrypt("vault:BOT_TOKEN").unwrap(),
            "synthetic-offline-token"
        );
        assert_eq!(
            fs::read_to_string(&calls).unwrap(),
            "vault status\nvault lease BOT_TOKEN --env-name REI_VAULT_SECRET --keychain --json\nvault status\nvault get BOT_TOKEN --reveal --json --keychain\nvault status\n"
        );

        // 7. Truly empty JSON secret values fail closed on both lease and keychain get.
        write_fake_tachi(
            tmp.path(),
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#,
            r#"{"leased":true,"env":{"REI_VAULT_SECRET":""}}"#,
            0,
            r#"{"name":"OPENAI_OFFICIAL_API_KEY","revealed":true,"value":"must-not-fallback"}"#,
            0,
        );
        assert!(store.decrypt("vault:OPENAI_OFFICIAL_API_KEY").is_err());

        write_fake_tachi_with_stderr(
            tmp.path(),
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#,
            "",
            "Error: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key",
            1,
            r#"{"name":"BOT_TOKEN","revealed":true,"value":""}"#,
            0,
        );
        assert!(store.decrypt("vault:BOT_TOKEN").is_err());

        assert!(!tmp.path().join(".secret_key").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tachi_vault_keychain_fallback_never_bypasses_lease_refusal_or_protocol_errors() {
        let _guard = crate::env_overrides::env_test_lock().await;
        let tmp = TempDir::new().unwrap();
        let _path = EnvValueGuard::set("PATH", tmp.path());
        let calls = tmp.path().join("calls");
        let _calls = EnvValueGuard::set("TACHI_TEST_CALLS", &calls);
        let store = SecretStore::new(tmp.path(), true);

        let unlocked_with_keychain =
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#;
        let locked_with_keychain =
            r#"{"initialized":true,"locked":true,"secure_store":{"auto_unlock_available":true}}"#;
        let offline_initialized = "Vault status:\n  initialized: true\n  entries: 3";
        let valid_get_out = r#"{"name":"OPENAI_OFFICIAL_API_KEY","revealed":true,"value":"synthetic-bypass-secret"}"#;

        let refusal_cases = [
            // Keychain available + disabled/auth-failed/rate-limited API key -> refuses, no `vault get`.
            (
                unlocked_with_keychain,
                "",
                "Error: No usable API key available for 'OPENAI_OFFICIAL_API_KEY'. Vault may be locked, missing, restricted, or all keys are disabled/auth-failed/rate-limited.",
                1u8,
                "no usable API key is eligible for lease",
                true,
            ),
            // Keychain available + malformed lease JSON -> refuses, no `vault get`.
            (
                unlocked_with_keychain,
                "{not-valid-json-synthetic-stdout-secret",
                "",
                0u8,
                "invalid lease JSON",
                true,
            ),
            // Keychain available + leased:false protocol refusal -> refuses, no `vault get`.
            (
                unlocked_with_keychain,
                r#"{"leased":false,"env":{"REI_VAULT_SECRET":"synthetic-stdout-secret"}}"#,
                "",
                0u8,
                "did not lease",
                true,
            ),
            // Keychain available + empty leased value -> refuses, no `vault get`.
            (
                unlocked_with_keychain,
                r#"{"leased":true,"env":{"REI_VAULT_SECRET":""}}"#,
                "",
                0u8,
                "empty secret",
                true,
            ),
            // Keychain available + missing string value in lease response -> refuses, no `vault get`.
            (
                unlocked_with_keychain,
                r#"{"leased":true,"env":{"REI_VAULT_SECRET":42}}"#,
                "",
                0u8,
                "missing string value",
                true,
            ),
            // Keychain available + generic/unknown lease CLI refusal -> refuses, no `vault get`.
            (
                unlocked_with_keychain,
                "",
                "lease refused: synthetic-stderr-secret",
                1u8,
                "cannot be leased",
                true,
            ),
            // Keychain available + locked daemon -> refuses offline Keychain bypass completely.
            (
                locked_with_keychain,
                "",
                "",
                1u8,
                "refusing offline Keychain bypass",
                false,
            ),
            // Offline mode + disabled key -> refuses, no `vault get`.
            (
                offline_initialized,
                "",
                "Error: No usable API key available for 'OPENAI_OFFICIAL_API_KEY'. All keys are disabled.",
                1u8,
                "no usable API key is eligible for lease",
                true,
            ),
            // Offline mode + malformed lease JSON -> refuses, no `vault get`.
            (
                offline_initialized,
                "synthetic-stdout-secret",
                "",
                0u8,
                "invalid lease JSON",
                true,
            ),
        ];

        for (status, lease_out, lease_stderr, lease_exit, expected, lease_called) in refusal_cases {
            fs::write(&calls, "").unwrap();
            write_fake_tachi_with_stderr(
                tmp.path(),
                status,
                lease_out,
                lease_stderr,
                lease_exit,
                valid_get_out,
                0,
            );
            let error = format!(
                "{:#}",
                store
                    .decrypt("vault:OPENAI_OFFICIAL_API_KEY")
                    .expect_err("must fail closed without Keychain bypass")
            );
            assert!(error.contains(expected), "unexpected error: {error}");
            assert!(!error.contains("synthetic-bypass-secret"), "{error}");
            assert!(!error.contains("synthetic-stdout-secret"), "{error}");
            assert!(!error.contains("synthetic-stderr-secret"), "{error}");
            let recorded = fs::read_to_string(&calls).unwrap();
            assert_eq!(recorded.contains("vault lease"), lease_called, "{recorded}");
            assert!(
                !recorded.contains("vault get"),
                "must never fall back to raw vault get: {recorded}"
            );
        }

        // Non-1 exit code (e.g., 65) with matching non-api_key stderr must fail closed.
        fs::write(&calls, "").unwrap();
        write_fake_tachi_with_stderr(
            tmp.path(),
            unlocked_with_keychain,
            "",
            "Fatal: Vault name 'OPENAI_OFFICIAL_API_KEY' is token, not a credential; refusing to lease it as an API key",
            65,
            valid_get_out,
            0,
        );
        let error = format!(
            "{:#}",
            store
                .decrypt("vault:OPENAI_OFFICIAL_API_KEY")
                .expect_err("non-1 exit code must not trigger Keychain fallback")
        );
        assert!(!error.contains("synthetic-bypass-secret"), "{error}");
        let recorded = fs::read_to_string(&calls).unwrap();
        assert!(!recorded.contains("vault get"), "{recorded}");

        // Multi-line conflicting stderr (e.g., Access denied + type refusal) must fail closed.
        for conflicting in [
            "Access denied\nVault name 'OPENAI_OFFICIAL_API_KEY' is token, not a credential; refusing to lease it as an API key",
            "Warning: something\nError: Vault name 'OPENAI_OFFICIAL_API_KEY' is token, not a credential; refusing to lease it as an API key",
            "Fatal: Vault name 'OPENAI_OFFICIAL_API_KEY' is token, not a credential; refusing to lease it as an API key\nAccess denied",
        ] {
            fs::write(&calls, "").unwrap();
            write_fake_tachi_with_stderr(
                tmp.path(),
                unlocked_with_keychain,
                "",
                conflicting,
                1,
                valid_get_out,
                0,
            );
            let error = format!(
                "{:#}",
                store
                    .decrypt("vault:OPENAI_OFFICIAL_API_KEY")
                    .expect_err("conflicting multi-line stderr must not trigger Keychain fallback")
            );
            assert!(!error.contains("synthetic-bypass-secret"), "{error}");
            let recorded = fs::read_to_string(&calls).unwrap();
            assert!(!recorded.contains("vault get"), "{recorded}");
        }

        // Non-empty stdout alongside exit 1 + type refusal must fail closed.
        fs::write(&calls, "").unwrap();
        write_fake_tachi_with_stderr(
            tmp.path(),
            unlocked_with_keychain,
            "synthetic-stdout-secret",
            "Fatal: Vault name 'OPENAI_OFFICIAL_API_KEY' is token, not a credential; refusing to lease it as an API key",
            1,
            valid_get_out,
            0,
        );
        assert!(store.decrypt("vault:OPENAI_OFFICIAL_API_KEY").is_err());
        assert!(!fs::read_to_string(&calls).unwrap().contains("vault get"));

        // Signal termination after printing type refusal must fail closed.
        fs::write(&calls, "").unwrap();
        write_fake_tachi_script(
            tmp.path(),
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$TACHI_TEST_CALLS"
if [ "$*" = 'vault status' ]; then
  printf '%s\n' '{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}'
  exit 0
fi
case "$*" in
  'vault lease '* )
    printf '%s\n' 'tachi: logging to synthetic-private-log-path' >&2
    printf '%s\n' "Fatal: Vault name 'OPENAI_OFFICIAL_API_KEY' is token, not a credential; refusing to lease it as an API key" >&2
    kill -9 $$
    ;;
esac
printf '%s\n' '{"name":"OPENAI_OFFICIAL_API_KEY","revealed":true,"value":"synthetic-bypass-secret"}'
"#,
        );
        let err = store
            .decrypt("vault:OPENAI_OFFICIAL_API_KEY")
            .expect_err("signal termination must fail closed");
        assert!(
            format!("{err:#}").contains("terminated by signal"),
            "{err:#}"
        );
        assert!(!fs::read_to_string(&calls).unwrap().contains("vault get"));

        // TOCTOU race 1: daemon locks between initial status/lease refusal and fallback `vault get`.
        let status_counter = tmp.path().join("status_count_pre");
        fs::write(&status_counter, "0").unwrap();
        fs::write(&calls, "").unwrap();
        let pre_script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$TACHI_TEST_CALLS"
printf '%s\n' 'tachi: logging to synthetic-private-log-path' >&2
if [ "$*" = 'vault status' ]; then
  IFS= read -r n < '{counter}' || n=0
  n=$((n + 1))
  printf '%s\n' "$n" > '{counter}'
  if [ "$n" -eq 1 ]; then
    printf '%s\n' '{unlocked}'
  else
    printf '%s\n' '{locked}'
  fi
  exit 0
fi
case "$*" in
  'vault lease '* )
    printf '%s\n' "Fatal: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key" >&2
    exit 1
    ;;
  'vault get '* )
    printf '%s\n' '{{"name":"BOT_TOKEN","revealed":true,"value":"synthetic-bypass-secret"}}'
    exit 0
    ;;
esac
exit 98
"#,
            counter = status_counter.display(),
            unlocked = unlocked_with_keychain,
            locked = locked_with_keychain,
        );
        write_fake_tachi_script(tmp.path(), &pre_script);
        let err = format!(
            "{:#}",
            store
                .decrypt("vault:BOT_TOKEN")
                .expect_err("daemon locked before get must fail closed")
        );
        assert!(err.contains("refusing offline Keychain bypass"), "{err}");
        assert!(!err.contains("synthetic-bypass-secret"), "{err}");
        assert!(!fs::read_to_string(&calls).unwrap().contains("vault get"));

        // TOCTOU race 2: daemon locks during `vault get`; post-get re-check refuses plaintext.
        let status_counter_post = tmp.path().join("status_count_post");
        fs::write(&status_counter_post, "0").unwrap();
        fs::write(&calls, "").unwrap();
        let post_script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$TACHI_TEST_CALLS"
printf '%s\n' 'tachi: logging to synthetic-private-log-path' >&2
if [ "$*" = 'vault status' ]; then
  IFS= read -r n < '{counter}' || n=0
  n=$((n + 1))
  printf '%s\n' "$n" > '{counter}'
  if [ "$n" -le 2 ]; then
    printf '%s\n' '{unlocked}'
  else
    printf '%s\n' '{locked}'
  fi
  exit 0
fi
case "$*" in
  'vault lease '* )
    printf '%s\n' "Fatal: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key" >&2
    exit 1
    ;;
  'vault get '* )
    printf '%s\n' '{{"name":"BOT_TOKEN","revealed":true,"value":"synthetic-bypass-secret"}}'
    exit 0
    ;;
esac
exit 98
"#,
            counter = status_counter_post.display(),
            unlocked = unlocked_with_keychain,
            locked = locked_with_keychain,
        );
        write_fake_tachi_script(tmp.path(), &post_script);
        let err = format!(
            "{:#}",
            store
                .decrypt("vault:BOT_TOKEN")
                .expect_err("daemon locked during get must fail closed")
        );
        assert!(err.contains("refusing offline Keychain bypass"), "{err}");
        assert!(!err.contains("synthetic-bypass-secret"), "{err}");

        // TOCTOU race 3 & 4: offline initial status transitions to locked daemon before or during `vault get`.
        let write_toctou_script = |counter: &Path, first_status: &str, switch_after: u8| {
            let sh_escape = |s: &str| s.replace('\'', "'\\''");
            let first_esc = sh_escape(first_status);
            let locked_esc = sh_escape(locked_with_keychain);
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$TACHI_TEST_CALLS"
printf '%s\n' 'tachi: logging to synthetic-private-log-path' >&2
if [ "$*" = 'vault status' ]; then
  IFS= read -r n < '{counter}' || n=0
  n=$((n + 1))
  printf '%s\n' "$n" > '{counter}'
  if [ "$n" -le {switch_after} ]; then
    printf '%s\n' '{first}'
  else
    printf '%s\n' '{locked}'
  fi
  exit 0
fi
case "$*" in
  'vault lease '* )
    printf '%s\n' "Fatal: Vault name 'BOT_TOKEN' is token, not a credential; refusing to lease it as an API key" >&2
    exit 1
    ;;
  'vault get '* )
    printf '%s\n' '{{"name":"BOT_TOKEN","revealed":true,"value":"synthetic-bypass-secret"}}'
    exit 0
    ;;
esac
exit 98
"#,
                counter = counter.display(),
                switch_after = switch_after,
                first = first_esc,
                locked = locked_esc,
            )
        };

        for (label, switch_after, expect_get_called) in [
            (
                "offline->locked daemon before get must fail closed",
                1u8,
                false,
            ),
            (
                "offline->locked daemon during get must fail closed",
                2u8,
                true,
            ),
        ] {
            let counter = tmp
                .path()
                .join(format!("status_count_offline_{switch_after}"));
            fs::write(&counter, "0").unwrap();
            fs::write(&calls, "").unwrap();
            write_fake_tachi_script(
                tmp.path(),
                &write_toctou_script(&counter, offline_initialized, switch_after),
            );
            let err = format!("{:#}", store.decrypt("vault:BOT_TOKEN").expect_err(label));
            assert!(err.contains("refusing offline Keychain bypass"), "{err}");
            assert!(!err.contains("synthetic-bypass-secret"), "{err}");
            assert_eq!(
                fs::read_to_string(&calls).unwrap().contains("vault get"),
                expect_get_called
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tachi_vault_failures_are_closed_and_do_not_leak_stdout() {
        let _guard = crate::env_overrides::env_test_lock().await;
        let tmp = TempDir::new().unwrap();
        let _path = EnvValueGuard::set("PATH", tmp.path());
        let calls = tmp.path().join("calls");
        let _calls = EnvValueGuard::set("TACHI_TEST_CALLS", &calls);
        let store = SecretStore::new(tmp.path(), true);
        let unlocked_with_keychain =
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":true}}"#;
        let unlocked_no_keychain =
            r#"{"initialized":true,"locked":false,"secure_store":{"auto_unlock_available":false}}"#;
        let non_api_key_stderr = "Error: Vault name 'TEST_KEY' is token, not a credential; refusing to lease it as an API key";
        let cases = [
            (
                r#"{"initialized":false,"locked":true}"#,
                "synthetic-stdout-secret",
                "lease refused: synthetic-stderr-secret",
                0,
                "synthetic-stdout-secret",
                0,
                "not initialized",
                false,
                false,
            ),
            (
                "Vault status:\n  initialized: false\n  entries: 0",
                "synthetic-stdout-secret",
                "lease refused: synthetic-stderr-secret",
                0,
                "synthetic-stdout-secret",
                0,
                "not initialized",
                false,
                false,
            ),
            (
                r#"{"initialized":true,"locked":true}"#,
                "synthetic-stdout-secret",
                "lease refused: synthetic-stderr-secret",
                0,
                "synthetic-stdout-secret",
                0,
                "locked",
                false,
                false,
            ),
            (
                r#"{"initialized":true,"locked":true,"secure_store":{"auto_unlock_available":false}}"#,
                "synthetic-stdout-secret",
                "lease refused: synthetic-stderr-secret",
                0,
                "synthetic-stdout-secret",
                0,
                "locked",
                false,
                false,
            ),
            (
                r#"{"initialized":true,"locked":true,"secure_store":{"auto_unlock_available":true}}"#,
                "synthetic-stdout-secret",
                "lease refused: synthetic-stderr-secret",
                0,
                "synthetic-stdout-secret",
                0,
                "locked",
                false,
                false,
            ),
            (
                "synthetic-stdout-secret",
                "",
                "",
                0,
                "",
                0,
                "unrecognized status output",
                false,
                false,
            ),
            (
                "{synthetic-stdout-secret",
                "",
                "",
                0,
                "",
                0,
                "invalid status JSON",
                false,
                false,
            ),
            (
                r#"{"initialized":true,"locked":"synthetic-stdout-secret"}"#,
                "",
                "",
                0,
                "",
                0,
                "boolean locked",
                false,
                false,
            ),
            (
                unlocked_no_keychain,
                "",
                non_api_key_stderr,
                1,
                "synthetic-stdout-secret",
                0,
                "automatic Keychain unlock is unavailable",
                true,
                false,
            ),
            (
                unlocked_with_keychain,
                "synthetic-stdout-secret",
                non_api_key_stderr,
                1,
                "synthetic-stdout-secret",
                0,
                "entry is not a leasable API key",
                true,
                false,
            ),
            (
                unlocked_no_keychain,
                "synthetic-stdout-secret",
                "",
                0,
                "",
                0,
                "invalid lease JSON",
                true,
                false,
            ),
            (
                unlocked_with_keychain,
                "synthetic-stdout-secret",
                "",
                0,
                r#"{"name":"TEST_KEY","revealed":true,"value":"synthetic-stdout-secret"}"#,
                0,
                "invalid lease JSON",
                true,
                false,
            ),
            (
                unlocked_with_keychain,
                "",
                non_api_key_stderr,
                1,
                "synthetic-stdout-secret",
                1,
                "not found in Tachi Vault",
                true,
                true,
            ),
            (
                unlocked_with_keychain,
                "",
                non_api_key_stderr,
                1,
                "synthetic-stdout-secret",
                0,
                "invalid secret JSON",
                true,
                true,
            ),
            (
                unlocked_with_keychain,
                "",
                non_api_key_stderr,
                1,
                r#"{"revealed":false,"value":"synthetic-stdout-secret"}"#,
                0,
                "did not reveal",
                true,
                true,
            ),
            (
                unlocked_with_keychain,
                "",
                non_api_key_stderr,
                1,
                r#"{"revealed":true,"value":""}"#,
                0,
                "empty secret",
                true,
                true,
            ),
            (
                unlocked_no_keychain,
                r#"{"leased":true,"env":{"REI_VAULT_SECRET":""}}"#,
                "",
                0,
                "",
                1,
                "empty secret",
                true,
                false,
            ),
            (
                unlocked_with_keychain,
                "",
                non_api_key_stderr,
                1,
                r#"{"revealed":true,"value":42}"#,
                0,
                "missing string value",
                true,
                true,
            ),
        ];
        for (
            status,
            lease_out,
            lease_stderr,
            lease_exit,
            get_out,
            get_exit,
            expected,
            lease_called,
            get_called,
        ) in cases
        {
            fs::write(&calls, "").unwrap();
            write_fake_tachi_with_stderr(
                tmp.path(),
                status,
                lease_out,
                lease_stderr,
                lease_exit,
                get_out,
                get_exit,
            );
            let error = format!("{:#}", store.decrypt("vault:TEST_KEY").unwrap_err());
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("synthetic-stdout-secret"), "{error}");
            assert!(!error.contains("synthetic-stderr-secret"), "{error}");
            assert!(!error.contains("synthetic-private-log-path"), "{error}");
            let calls = fs::read_to_string(&calls).unwrap();
            assert_eq!(calls.contains("vault lease"), lease_called, "{calls}");
            assert_eq!(calls.contains("vault get"), get_called, "{calls}");
        }

        // Oversized pipe output fails closed without leaking stdout (pure sh builtins; PATH=tmp).
        write_fake_tachi_script(
            tmp.path(),
            "#!/bin/sh\ns='SSSSSSSSSSSSSSSS'\ns=\"$s$s$s$s\"\ns=\"$s$s$s$s\"\ns=\"$s$s$s$s\"\ns=\"$s$s$s$s\"\ns=\"$s$s$s$s\"\ns=\"$s$s$s$s\"\nprintf '%sS' \"$s\"\n",
        );
        let error = format!("{:#}", store.decrypt("vault:TEST_KEY").unwrap_err());
        assert!(error.contains("exceeded 64 KiB limit"), "{error}");
        assert!(!error.contains("SSSS"), "{error}");

        fs::remove_file(tmp.path().join("tachi")).unwrap();
        let error = store.decrypt("vault:TEST_KEY").unwrap_err().to_string();
        assert_eq!(
            error,
            "Tachi CLI (`tachi`) not found. Install it to use vault: secret references in config."
        );
        assert!(!tmp.path().join(".secret_key").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tachi_vault_timeout_is_bounded() {
        let _guard = crate::env_overrides::env_test_lock().await;
        let tmp = TempDir::new().unwrap();
        write_fake_op(tmp.path(), "#!/bin/sh\nexec /bin/sleep 30\n");
        fs::rename(tmp.path().join("op"), tmp.path().join("tachi")).unwrap();
        let _path = EnvValueGuard::set("PATH", tmp.path());
        let store = SecretStore::new(tmp.path(), true);
        let started = Instant::now();
        let error = store.decrypt("vault:TEST_KEY").unwrap_err().to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    // ── SecretStore basics ─────────────────────────────────────

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let secret = "sk-my-secret-api-key-12345";

        let encrypted = store.encrypt(secret).unwrap();
        assert!(encrypted.starts_with("enc2:"), "Should have enc2: prefix");
        assert_ne!(encrypted, secret, "Should not be plaintext");

        let decrypted = store.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, secret, "Roundtrip must preserve original");
    }

    #[test]
    fn encrypt_empty_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let result = store.encrypt("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn decrypt_plaintext_passthrough() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        // Values without "enc:"/"enc2:" prefix are returned as-is (backward compat)
        let result = store.decrypt("sk-plaintext-key").unwrap();
        assert_eq!(result, "sk-plaintext-key");
    }

    #[test]
    fn disabled_store_returns_plaintext() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), false);
        let result = store.encrypt("sk-secret").unwrap();
        assert_eq!(result, "sk-secret", "Disabled store should not encrypt");
    }

    #[test]
    fn is_encrypted_detects_prefix() {
        assert!(SecretStore::is_encrypted("enc2:aabbcc"));
        assert!(SecretStore::is_encrypted("enc:aabbcc")); // legacy
        assert!(SecretStore::is_encrypted("op://vault/item/field"));
        assert!(!SecretStore::is_encrypted("sk-plaintext"));
        assert!(!SecretStore::is_encrypted(""));
    }

    #[test]
    fn op_reference_invalid_format_fails_before_plaintext_passthrough() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let err = store.decrypt("op://vault-only").unwrap_err().to_string();

        assert!(err.contains("Invalid 1Password reference"));
    }

    #[test]
    fn op_reference_decrypt_and_migrate_does_not_migrate_or_pass_through() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let err = store
            .decrypt_and_migrate("op://vault-only")
            .unwrap_err()
            .to_string();

        assert!(err.contains("Invalid 1Password reference"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn op_reference_drains_stderr_while_waiting() {
        let _guard = crate::env_overrides::env_test_lock().await;
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        write_fake_op(
            &bin_dir,
            r#"#!/bin/sh
if [ "$1" = "read" ] && [ "$2" = "op://vault/item/field" ]; then
  yes diagnostic-line >&2 &
  spam_pid=$!
  sleep 1
  kill "$spam_pid"
  wait "$spam_pid" 2>/dev/null
  printf '%s\n' 'secret-from-op'
  exit 0
fi
exit 65
"#,
        );
        let path = match std::env::var_os("PATH") {
            Some(existing) if !existing.is_empty() => {
                format!("{}:{}", bin_dir.display(), existing.to_string_lossy())
            }
            _ => bin_dir.display().to_string(),
        };
        let _path_guard = EnvValueGuard::set("PATH", path);
        let store = SecretStore::new(tmp.path(), true);

        let secret = store.decrypt("op://vault/item/field").unwrap();

        assert_eq!(secret, "secret-from-op");
    }

    #[tokio::test]
    async fn key_file_created_on_first_encrypt() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        assert!(!store.key_path.exists());

        store.encrypt("test").unwrap();
        assert!(store.key_path.exists(), "Key file should be created");

        let key_hex = tokio::fs::read_to_string(&store.key_path).await.unwrap();
        assert_eq!(
            key_hex.len(),
            KEY_LEN * 2,
            "Key should be {KEY_LEN} bytes hex-encoded"
        );
    }

    #[test]
    fn encrypting_same_value_produces_different_ciphertext() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let e1 = store.encrypt("secret").unwrap();
        let e2 = store.encrypt("secret").unwrap();
        assert_ne!(
            e1, e2,
            "AEAD with random nonce should produce different ciphertext each time"
        );

        // Both should still decrypt to the same value
        assert_eq!(store.decrypt(&e1).unwrap(), "secret");
        assert_eq!(store.decrypt(&e2).unwrap(), "secret");
    }

    #[test]
    fn different_stores_same_dir_interop() {
        let tmp = TempDir::new().unwrap();
        let store1 = SecretStore::new(tmp.path(), true);
        let store2 = SecretStore::new(tmp.path(), true);

        let encrypted = store1.encrypt("cross-store-secret").unwrap();
        let decrypted = store2.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, "cross-store-secret");
    }

    #[test]
    fn unicode_secret_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let secret = "sk-日本語テスト-émojis-🦀";

        let encrypted = store.encrypt(secret).unwrap();
        let decrypted = store.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn long_secret_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let secret = "a".repeat(10_000);

        let encrypted = store.encrypt(&secret).unwrap();
        let decrypted = store.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn corrupt_hex_returns_error() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let result = store.decrypt("enc2:not-valid-hex!!");
        assert!(result.is_err());
    }

    #[test]
    fn tampered_ciphertext_detected() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let encrypted = store.encrypt("sensitive-data").unwrap();

        // Flip a bit in the ciphertext (after the "enc2:" prefix)
        let hex_str = &encrypted[5..];
        let mut blob = hex_decode(hex_str).unwrap();
        // Modify a byte in the ciphertext portion (after the 12-byte nonce)
        if blob.len() > NONCE_LEN {
            blob[NONCE_LEN] ^= 0xff;
        }
        let tampered = format!("enc2:{}", hex_encode(&blob));

        let result = store.decrypt(&tampered);
        assert!(result.is_err(), "Tampered ciphertext must be rejected");
    }

    #[test]
    fn wrong_key_detected() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let store1 = SecretStore::new(tmp1.path(), true);
        let store2 = SecretStore::new(tmp2.path(), true);

        let encrypted = store1.encrypt("secret-for-store1").unwrap();
        let result = store2.decrypt(&encrypted);
        assert!(result.is_err(), "Decrypting with a different key must fail");
    }

    #[test]
    fn decrypt_error_message_mentions_secret_key() {
        // Operators hitting a missing or mismatched `.secret_key` (volume wipe,
        // container migration, backup-restore without the key file) need the
        // error message to point at the root cause. Otherwise the failure
        // cascades into a misleading "All providers/models failed" message
        // with no diagnostic for the underlying decrypt failure.
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let store1 = SecretStore::new(tmp1.path(), true);
        let store2 = SecretStore::new(tmp2.path(), true);

        let encrypted = store1.encrypt("secret-for-store1").unwrap();
        let err = store2.decrypt(&encrypted).expect_err("wrong key must fail");
        let msg = err.to_string();
        assert!(
            msg.contains(".secret_key"),
            "decrypt error must mention `.secret_key` so operators can diagnose missing/mismatched keys: got {msg:?}"
        );
    }

    #[test]
    fn truncated_ciphertext_returns_error() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        // Only a few bytes — shorter than nonce
        let result = store.decrypt("enc2:aabbccdd");
        assert!(result.is_err(), "Too-short ciphertext must be rejected");
    }

    // ── Legacy XOR backward compatibility ───────────────────────

    #[test]
    fn legacy_xor_decrypt_still_works() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        // Trigger key creation via an encrypt call
        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        // Manually produce a legacy XOR-encrypted value
        let plaintext = "sk-legacy-api-key";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        // Store should still be able to decrypt legacy values
        let decrypted = store.decrypt(&legacy_value).unwrap();
        assert_eq!(decrypted, plaintext, "Legacy XOR values must still decrypt");
    }

    // ── Migration tests ─────────────────────────────────────────

    #[test]
    fn needs_migration_detects_legacy_prefix() {
        assert!(SecretStore::needs_migration("enc:aabbcc"));
        assert!(!SecretStore::needs_migration("enc2:aabbcc"));
        assert!(!SecretStore::needs_migration("sk-plaintext"));
        assert!(!SecretStore::needs_migration(""));
    }

    #[test]
    fn is_secure_encrypted_detects_enc2_only() {
        assert!(SecretStore::is_secure_encrypted("enc2:aabbcc"));
        assert!(!SecretStore::is_secure_encrypted("enc:aabbcc"));
        assert!(!SecretStore::is_secure_encrypted("sk-plaintext"));
        assert!(!SecretStore::is_secure_encrypted(""));
    }

    #[test]
    fn decrypt_and_migrate_returns_none_for_enc2() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let encrypted = store.encrypt("my-secret").unwrap();
        assert!(encrypted.starts_with("enc2:"));

        let (plaintext, migrated) = store.decrypt_and_migrate(&encrypted).unwrap();
        assert_eq!(plaintext, "my-secret");
        assert!(
            migrated.is_none(),
            "enc2: values should not trigger migration"
        );
    }

    #[test]
    fn decrypt_and_migrate_returns_none_for_plaintext() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let (plaintext, migrated) = store.decrypt_and_migrate("sk-plaintext-key").unwrap();
        assert_eq!(plaintext, "sk-plaintext-key");
        assert!(
            migrated.is_none(),
            "Plaintext values should not trigger migration"
        );
    }

    #[test]
    fn decrypt_and_migrate_upgrades_legacy_xor() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        // Create key first
        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        // Manually create a legacy XOR-encrypted value
        let plaintext = "sk-legacy-secret-to-migrate";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        // Verify it needs migration
        assert!(SecretStore::needs_migration(&legacy_value));

        // Decrypt and migrate
        let (decrypted, migrated) = store.decrypt_and_migrate(&legacy_value).unwrap();
        assert_eq!(decrypted, plaintext, "Plaintext must match original");
        assert!(migrated.is_some(), "Legacy value should trigger migration");

        let new_value = migrated.unwrap();
        assert!(
            new_value.starts_with("enc2:"),
            "Migrated value must use enc2: prefix"
        );
        assert!(
            !SecretStore::needs_migration(&new_value),
            "Migrated value should not need migration"
        );

        // Verify the migrated value decrypts correctly
        let (decrypted2, migrated2) = store.decrypt_and_migrate(&new_value).unwrap();
        assert_eq!(
            decrypted2, plaintext,
            "Migrated value must decrypt to same plaintext"
        );
        assert!(
            migrated2.is_none(),
            "Migrated value should not trigger another migration"
        );
    }

    #[test]
    fn decrypt_and_migrate_handles_unicode() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        let plaintext = "sk-日本語-émojis-🦀-тест";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        let (decrypted, migrated) = store.decrypt_and_migrate(&legacy_value).unwrap();
        assert_eq!(decrypted, plaintext);
        assert!(migrated.is_some());

        // Verify migrated value works
        let new_value = migrated.unwrap();
        let (decrypted2, _) = store.decrypt_and_migrate(&new_value).unwrap();
        assert_eq!(decrypted2, plaintext);
    }

    #[test]
    fn decrypt_and_migrate_handles_empty_secret() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        // Empty plaintext XOR-encrypted
        let plaintext = "";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        let (decrypted, migrated) = store.decrypt_and_migrate(&legacy_value).unwrap();
        assert_eq!(decrypted, plaintext);
        // Empty string encryption returns empty string (not enc2:)
        assert!(migrated.is_some());
        assert_eq!(migrated.unwrap(), "");
    }

    #[test]
    fn decrypt_and_migrate_handles_long_secret() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        let plaintext = "a".repeat(10_000);
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        let (decrypted, migrated) = store.decrypt_and_migrate(&legacy_value).unwrap();
        assert_eq!(decrypted, plaintext);
        assert!(migrated.is_some());

        let new_value = migrated.unwrap();
        let (decrypted2, _) = store.decrypt_and_migrate(&new_value).unwrap();
        assert_eq!(decrypted2, plaintext);
    }

    #[test]
    fn decrypt_and_migrate_fails_on_corrupt_legacy_hex() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let _ = store.encrypt("setup").unwrap();

        let result = store.decrypt_and_migrate("enc:not-valid-hex!!");
        assert!(result.is_err(), "Corrupt hex should fail");
    }

    #[test]
    fn decrypt_and_migrate_wrong_key_produces_garbage_or_fails() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let store1 = SecretStore::new(tmp1.path(), true);
        let store2 = SecretStore::new(tmp2.path(), true);

        // Create keys for both stores
        let _ = store1.encrypt("setup").unwrap();
        let _ = store2.encrypt("setup").unwrap();
        let key1 = store1.load_or_create_key().unwrap();

        // Encrypt with store1's key
        let plaintext = "secret-for-store1";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key1);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        // Decrypt with store2 — XOR will produce garbage bytes
        // This may fail with UTF-8 error or succeed with garbage plaintext
        match store2.decrypt_and_migrate(&legacy_value) {
            Ok((decrypted, _)) => {
                // If it succeeds, the plaintext should be garbage (not the original)
                assert_ne!(
                    decrypted, plaintext,
                    "Wrong key should produce garbage plaintext"
                );
            }
            Err(e) => {
                // Expected: UTF-8 decoding failure from garbage bytes
                assert!(
                    e.to_string().contains("UTF-8"),
                    "Error should be UTF-8 related: {e}"
                );
            }
        }
    }

    #[test]
    fn migration_produces_different_ciphertext_each_time() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        let plaintext = "sk-same-secret";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        let (_, migrated1) = store.decrypt_and_migrate(&legacy_value).unwrap();
        let (_, migrated2) = store.decrypt_and_migrate(&legacy_value).unwrap();

        assert!(migrated1.is_some());
        assert!(migrated2.is_some());
        assert_ne!(
            migrated1.unwrap(),
            migrated2.unwrap(),
            "Each migration should produce different ciphertext (random nonce)"
        );
    }

    #[test]
    fn migrated_value_is_tamper_resistant() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);

        let _ = store.encrypt("setup").unwrap();
        let key = store.load_or_create_key().unwrap();

        let plaintext = "sk-sensitive-data";
        let ciphertext = xor_cipher(plaintext.as_bytes(), &key);
        let legacy_value = format!("enc:{}", hex_encode(&ciphertext));

        let (_, migrated) = store.decrypt_and_migrate(&legacy_value).unwrap();
        let new_value = migrated.unwrap();

        // Tamper with the migrated value
        let hex_str = &new_value[5..];
        let mut blob = hex_decode(hex_str).unwrap();
        if blob.len() > NONCE_LEN {
            blob[NONCE_LEN] ^= 0xff;
        }
        let tampered = format!("enc2:{}", hex_encode(&blob));

        let result = store.decrypt_and_migrate(&tampered);
        assert!(result.is_err(), "Tampered migrated value must be rejected");
    }

    // ── Low-level helpers ───────────────────────────────────────

    #[test]
    fn xor_cipher_roundtrip() {
        let key = b"testkey123";
        let data = b"hello world";
        let encrypted = xor_cipher(data, key);
        let decrypted = xor_cipher(&encrypted, key);
        assert_eq!(decrypted, data);
    }

    #[test]
    fn xor_cipher_empty_key() {
        let data = b"passthrough";
        let result = xor_cipher(data, &[]);
        assert_eq!(result, data);
    }

    #[test]
    fn hex_roundtrip() {
        let data = vec![0x00, 0x01, 0xfe, 0xff, 0xab, 0xcd];
        let encoded = hex_encode(&data);
        assert_eq!(encoded, "0001feffabcd");
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn hex_decode_odd_length_fails() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn hex_decode_invalid_chars_fails() {
        assert!(hex_decode("zzzz").is_err());
    }

    #[test]
    fn hex_decode_non_ascii_returns_error() {
        // A corrupt/tampered ciphertext with even *byte* length made of
        // non-ASCII chars previously slipped past the odd-length check and
        // panicked on mid-UTF-8-char byte slicing. It must now return Err
        // gracefully (the signature's promise). "€€" is 6 bytes.
        assert!(hex_decode("€€").is_err());
        // Non-ASCII that is a whole 2-byte char also errors, not panics.
        assert!(hex_decode("ÿÿ").is_err());
    }

    #[test]
    fn generate_random_key_correct_length() {
        let key = generate_random_key();
        assert_eq!(key.len(), KEY_LEN);
    }

    #[test]
    fn generate_random_key_not_all_zeros() {
        let key = generate_random_key();
        assert!(key.iter().any(|&b| b != 0), "Key should not be all zeros");
    }

    #[test]
    fn two_random_keys_differ() {
        let k1 = generate_random_key();
        let k2 = generate_random_key();
        assert_ne!(k1, k2, "Two random keys should differ");
    }

    #[test]
    fn generate_random_key_has_no_uuid_fixed_bits() {
        // UUID v4 has fixed bits at positions 6 (version = 0b0100xxxx) and
        // 8 (variant = 0b10xxxxxx). A direct CSPRNG key should not consistently
        // have these patterns across multiple samples.
        let mut version_match = 0;
        let mut variant_match = 0;
        let samples = 100;
        for _ in 0..samples {
            let key = generate_random_key();
            // In UUID v4, byte 6 always has top nibble = 0x4
            if key[6] & 0xf0 == 0x40 {
                version_match += 1;
            }
            // In UUID v4, byte 8 always has top 2 bits = 0b10
            if key[8] & 0xc0 == 0x80 {
                variant_match += 1;
            }
        }
        // With true randomness, each pattern should appear ~1/16 and ~1/4 of
        // the time. UUID would hit 100/100 on both. Allow generous margin.
        assert!(
            version_match < 30,
            "byte[6] matched UUID v4 version nibble {version_match}/100 times — \
             likely still using UUID-based key generation"
        );
        assert!(
            variant_match < 50,
            "byte[8] matched UUID v4 variant bits {variant_match}/100 times — \
             likely still using UUID-based key generation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_file_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        store.encrypt("trigger key creation").unwrap();

        let perms = fs::metadata(&store.key_path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "Key file must be owner-only (0600)"
        );
    }

    #[test]
    fn decrypt_existing_fails_closed_on_missing_or_short_key_without_creating() {
        let tmp = TempDir::new().unwrap();
        let store = SecretStore::new(tmp.path(), true);
        let missing_err = store
            .decrypt_existing("enc2:000102030405060708090a0b0c0d0e0f1011")
            .expect_err("missing .secret_key must fail");
        assert!(!missing_err.to_string().is_empty());
        assert!(!store.key_path.exists());

        fs::write(&store.key_path, "deadbeef").unwrap();
        let short_err = store
            .decrypt_existing("enc2:000102030405060708090a0b0c0d0e0f1011")
            .expect_err("short .secret_key must fail without panic");
        assert!(short_err.to_string().contains("invalid length"));
    }
}
