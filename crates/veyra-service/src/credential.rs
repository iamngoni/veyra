//! Encrypted console-entered model credential.
//!
//! The ordinary runtime settings overlay rejects secrets because it is
//! readable and audited. This separate boundary authenticates changes with
//! an operator token, encrypts before durable storage, and never returns the
//! plaintext. A configured vault requires a database so saving cannot silently
//! degrade into a process-only key.

use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::model::ApiKey;

const KEY_ENV: &str = "VEYRA_CONSOLE_SECRET_KEY";
const TOKEN_ENV: &str = "VEYRA_CONSOLE_ADMIN_TOKEN";
const AAD: &[u8] = b"veyra:model-credential:v1";

struct VaultInner {
    cipher: LessSafeKey,
    admin_token: String,
}

/// Cloneable operator credential vault. Debug output never includes secrets.
#[derive(Clone)]
pub struct CredentialVault(Arc<VaultInner>);

impl fmt::Debug for CredentialVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialVault(redacted)")
    }
}

impl CredentialVault {
    /// Reads the encryption key and operator token as one configuration unit.
    /// Neither may be set alone, and the key must decode to 32 bytes.
    ///
    /// # Errors
    /// Returns a non-sensitive configuration explanation.
    pub fn from_env() -> Result<Option<Self>, String> {
        let key = std::env::var(KEY_ENV).unwrap_or_default();
        let token = std::env::var(TOKEN_ENV).unwrap_or_default();
        if key.is_empty() && token.is_empty() {
            return Ok(None);
        }
        if key.is_empty() || token.chars().count() < 32 {
            return Err(format!(
                "{KEY_ENV} and {TOKEN_ENV} must both be configured; the admin token needs at least 32 characters"
            ));
        }
        let bytes = STANDARD
            .decode(key)
            .map_err(|_| format!("{KEY_ENV} must be a base64-encoded 32-byte key"))?;
        let cipher = UnboundKey::new(&aead::CHACHA20_POLY1305, &bytes)
            .map_err(|_| format!("{KEY_ENV} must be a base64-encoded 32-byte key"))?;
        Ok(Some(Self(Arc::new(VaultInner {
            cipher: LessSafeKey::new(cipher),
            admin_token: token,
        }))))
    }

    /// Checks the supplied operator token without leaking a matching prefix.
    pub fn authenticates(&self, supplied: &str) -> bool {
        self.0
            .admin_token
            .as_bytes()
            .ct_eq(supplied.as_bytes())
            .unwrap_u8()
            == 1
    }

    /// Encrypts a validated API key for the durable state table.
    ///
    /// # Errors
    /// Returns a non-sensitive error if secure randomness or encryption fails.
    pub fn seal(&self, key: &ApiKey) -> Result<Value, String> {
        self.seal_text(key.expose())
    }

    /// Encrypts an opaque provider credential for durable storage.
    ///
    /// The caller remains responsible for validating the credential format;
    /// this boundary only encrypts bytes and never exposes them in responses.
    pub fn seal_text(&self, secret: &str) -> Result<Value, String> {
        let mut nonce_bytes = [0_u8; 12];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| "secure randomness unavailable".to_owned())?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut ciphertext = secret.as_bytes().to_vec();
        self.0
            .cipher
            .seal_in_place_append_tag(nonce, Aad::from(AAD), &mut ciphertext)
            .map_err(|_| "credential encryption failed".to_owned())?;
        let mut packed = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
        packed.extend_from_slice(&nonce_bytes);
        packed.extend_from_slice(&ciphertext);
        Ok(json!({"version": 1, "ciphertext": STANDARD.encode(packed)}))
    }

    /// Decrypts a stored credential, or a tombstone that clears the override.
    /// Corrupt or wrong-key state fails startup instead of silently dropping it.
    ///
    /// # Errors
    /// Returns a non-sensitive explanation for malformed or undecryptable data.
    pub fn open(&self, stored: &Value) -> Result<Option<ApiKey>, String> {
        self.open_text(stored)?
            .map(|secret| {
                ApiKey::parse(&secret).map_err(|_| "credential state is malformed".to_owned())
            })
            .transpose()
    }

    /// Decrypts an opaque provider credential from durable storage.
    pub fn open_text(&self, stored: &Value) -> Result<Option<String>, String> {
        if stored.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("unsupported credential state version".to_owned());
        }
        let Some(encoded) = stored.get("ciphertext").and_then(Value::as_str) else {
            if stored.get("ciphertext").is_some_and(Value::is_null) {
                return Ok(None);
            }
            return Err("credential state is malformed".to_owned());
        };
        let mut packed = STANDARD
            .decode(encoded)
            .map_err(|_| "credential state is malformed".to_owned())?;
        if packed.len() < 12 + aead::CHACHA20_POLY1305.tag_len() {
            return Err("credential state is malformed".to_owned());
        }
        let nonce_bytes: [u8; 12] = packed[..12]
            .try_into()
            .map_err(|_| "credential state is malformed".to_owned())?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let plain = self
            .0
            .cipher
            .open_in_place(nonce, Aad::from(AAD), &mut packed[12..])
            .map_err(|_| "credential state cannot be decrypted".to_owned())?;
        let text =
            std::str::from_utf8(plain).map_err(|_| "credential state is malformed".to_owned())?;
        Ok(Some(text.to_owned()))
    }
}

/// Operator token accepted by [`test_vault`].
#[cfg(test)]
pub(crate) const TEST_ADMIN_TOKEN: &str = "operator-token-with-more-than-32-characters";

/// A vault with a fixed test key, for crate tests that need encrypted state
/// without reading the process environment.
#[cfg(test)]
pub(crate) fn test_vault() -> CredentialVault {
    match UnboundKey::new(&aead::CHACHA20_POLY1305, &[7_u8; 32]) {
        Ok(key) => CredentialVault(Arc::new(VaultInner {
            cipher: LessSafeKey::new(key),
            admin_token: TEST_ADMIN_TOKEN.to_owned(),
        })),
        Err(_) => panic!("test key must be accepted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> CredentialVault {
        let key = UnboundKey::new(&aead::CHACHA20_POLY1305, &[7_u8; 32]);
        match key {
            Ok(key) => CredentialVault(Arc::new(VaultInner {
                cipher: LessSafeKey::new(key),
                admin_token: "operator-token-with-more-than-32-characters".to_owned(),
            })),
            Err(_) => panic!("test key must be accepted"),
        }
    }

    #[test]
    fn round_trip_is_encrypted_and_redacted() {
        let vault = vault();
        let key = ApiKey::parse("sensitive-provider-key").expect("valid test key");
        let stored = vault.seal(&key).expect("encrypt");
        assert!(!stored.to_string().contains(key.expose()));
        assert!(!format!("{vault:?}").contains("operator-token"));
        assert_eq!(vault.open(&stored).expect("decrypt"), Some(key));
        assert!(vault.authenticates("operator-token-with-more-than-32-characters"));
        assert!(!vault.authenticates("operator-token-with-more-than-32-characterx"));
    }

    #[test]
    fn tampering_does_not_return_a_key() {
        let vault = vault();
        assert!(
            vault
                .open(&json!({"version": 1, "ciphertext": "AAAA"}))
                .is_err()
        );
        assert_eq!(
            vault
                .open(&json!({"version": 1, "ciphertext": null}))
                .expect("tombstone"),
            None
        );
    }
}
