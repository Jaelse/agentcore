//! Envelope for secrets at rest: AES-256-GCM with a 256-bit master key.

use std::path::Path;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::StoreError;

pub const MASTER_KEY_ENV: &str = "AGENTCORE_MASTER_KEY";

#[derive(Clone)]
pub struct Cipher {
    aead: Aes256Gcm,
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(..)")
    }
}

impl Cipher {
    pub fn from_key(key: &[u8; 32]) -> Self {
        Self {
            aead: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
        }
    }

    fn decode(encoded: &str) -> Result<Self, StoreError> {
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|e| StoreError::MasterKey(format!("not valid base64: {e}")))?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| StoreError::MasterKey("must be 32 bytes (base64-encoded)".into()))?;
        Ok(Self::from_key(&key))
    }

    /// Load the master key from `$AGENTCORE_MASTER_KEY`, else from `file`,
    /// generating the file (mode 0600) on first start.
    pub fn load_or_create(file: &Path) -> Result<Self, StoreError> {
        // An empty variable (e.g. docker-compose `${VAR:-}`) counts as unset.
        if let Ok(encoded) = std::env::var(MASTER_KEY_ENV)
            && !encoded.trim().is_empty()
        {
            return Self::decode(&encoded);
        }
        match std::fs::read_to_string(file) {
            Ok(encoded) => Self::decode(&encoded),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Aes256Gcm::generate_key(OsRng);
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| StoreError::MasterKey(e.to_string()))?;
                }
                write_private(file, STANDARD.encode(key).as_bytes())
                    .map_err(|e| StoreError::MasterKey(format!("{}: {e}", file.display())))?;
                tracing::warn!(
                    path = %file.display(),
                    "generated a new master key; back it up, encrypted secrets are unreadable without it"
                );
                Ok(Self {
                    aead: Aes256Gcm::new(&key),
                })
            }
            Err(e) => Err(StoreError::MasterKey(format!("{}: {e}", file.display()))),
        }
    }

    /// Returns `(nonce, ciphertext)`.
    pub fn encrypt(&self, plaintext: &[u8], aad: &[u8]) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .aead
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| StoreError::Crypto)?;
        Ok((nonce.to_vec(), ciphertext))
    }

    pub fn decrypt(
        &self,
        nonce: &[u8],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, StoreError> {
        if nonce.len() != 12 {
            return Err(StoreError::Crypto);
        }
        self.aead
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| StoreError::Crypto)
    }
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_aad_binding() {
        let cipher = Cipher::from_key(&[7; 32]);
        let (nonce, ct) = cipher.encrypt(b"sk-secret", b"anthropic").unwrap();
        assert_eq!(
            cipher.decrypt(&nonce, &ct, b"anthropic").unwrap(),
            b"sk-secret"
        );
        assert!(cipher.decrypt(&nonce, &ct, b"openai").is_err());
        assert!(
            Cipher::from_key(&[8; 32])
                .decrypt(&nonce, &ct, b"anthropic")
                .is_err()
        );
    }

    #[test]
    fn generates_and_reloads_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/master.key");
        let a = Cipher::load_or_create(&path).unwrap();
        let (nonce, ct) = a.encrypt(b"x", b"").unwrap();
        let b = Cipher::load_or_create(&path).unwrap();
        assert_eq!(b.decrypt(&nonce, &ct, b"").unwrap(), b"x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
