use mqtt_transport::PreSharedKey;

use crate::types::KeychainSecretId;
use thiserror::Error;

const KEYCHAIN_SERVICE: &str = "dev.tyde.mobile.psk";

pub trait PskStore: Send + Sync {
    fn store(&self, psk: &PreSharedKey) -> Result<KeychainSecretId, PskStoreError>;
    fn load(&self, key_id: &KeychainSecretId) -> Result<PreSharedKey, PskStoreError>;
    fn delete(&self, key_id: &KeychainSecretId) -> Result<(), PskStoreError>;
}

#[derive(Debug, Error)]
pub enum PskStoreError {
    #[error("failed to decode PSK from keychain entry {key_id}: {message}")]
    Decode {
        key_id: KeychainSecretId,
        message: String,
    },
    #[error("keychain operation {operation} failed for {key_id}: {message}")]
    Backend {
        operation: &'static str,
        key_id: KeychainSecretId,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct SystemPskStore;

impl SystemPskStore {
    pub fn new() -> Self {
        Self
    }
}

impl PskStore for SystemPskStore {
    fn store(&self, psk: &PreSharedKey) -> Result<KeychainSecretId, PskStoreError> {
        let key_id = new_keychain_secret_id();
        platform::store_secret(&key_id, psk.as_bytes())?;
        Ok(key_id)
    }

    fn load(&self, key_id: &KeychainSecretId) -> Result<PreSharedKey, PskStoreError> {
        let bytes = platform::load_secret(key_id)?;
        PreSharedKey::from_slice(&bytes).map_err(|error| PskStoreError::Decode {
            key_id: key_id.clone(),
            message: error.to_string(),
        })
    }

    fn delete(&self, key_id: &KeychainSecretId) -> Result<(), PskStoreError> {
        platform::delete_secret(key_id)
    }
}

fn new_keychain_secret_id() -> KeychainSecretId {
    KeychainSecretId(format!("tyde-mobile-psk-{}", uuid::Uuid::new_v4()))
}

#[cfg(target_os = "ios")]
mod platform {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::{KEYCHAIN_SERVICE, KeychainSecretId, PskStoreError, key_id_str};

    pub(super) fn store_secret(
        key_id: &KeychainSecretId,
        bytes: &[u8],
    ) -> Result<(), PskStoreError> {
        set_generic_password(KEYCHAIN_SERVICE, key_id_str(key_id), bytes).map_err(|error| {
            PskStoreError::Backend {
                operation: "store",
                key_id: key_id.clone(),
                message: error.to_string(),
            }
        })
    }

    pub(super) fn load_secret(key_id: &KeychainSecretId) -> Result<Vec<u8>, PskStoreError> {
        get_generic_password(KEYCHAIN_SERVICE, key_id_str(key_id)).map_err(|error| {
            PskStoreError::Backend {
                operation: "load",
                key_id: key_id.clone(),
                message: error.to_string(),
            }
        })
    }

    pub(super) fn delete_secret(key_id: &KeychainSecretId) -> Result<(), PskStoreError> {
        delete_generic_password(KEYCHAIN_SERVICE, key_id_str(key_id)).map_err(|error| {
            PskStoreError::Backend {
                operation: "delete",
                key_id: key_id.clone(),
                message: error.to_string(),
            }
        })
    }
}

#[cfg(all(not(target_os = "ios"), not(target_os = "android"), debug_assertions))]
mod platform {
    use std::io;
    use std::path::{Path, PathBuf};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::{KEYCHAIN_SERVICE, KeychainSecretId, PskStoreError, safe_file_name};

    pub(super) fn store_secret(
        key_id: &KeychainSecretId,
        bytes: &[u8],
    ) -> Result<(), PskStoreError> {
        let path = path_for(key_id, "store")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| backend("store", key_id, error))?;
        }
        let temp_path = path.with_file_name(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("psk"),
            uuid::Uuid::new_v4()
        ));
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        std::fs::write(&temp_path, encoded.as_bytes())
            .map_err(|error| backend("store", key_id, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| backend("store", key_id, error))?;
        }
        std::fs::rename(&temp_path, &path).map_err(|error| backend("store", key_id, error))
    }

    pub(super) fn load_secret(key_id: &KeychainSecretId) -> Result<Vec<u8>, PskStoreError> {
        let path = path_for(key_id, "load")?;
        let encoded =
            std::fs::read_to_string(&path).map_err(|error| backend("load", key_id, error))?;
        URL_SAFE_NO_PAD
            .decode(encoded.trim().as_bytes())
            .map_err(|error| PskStoreError::Decode {
                key_id: key_id.clone(),
                message: format!("invalid dev keychain base64: {error}"),
            })
    }

    pub(super) fn delete_secret(key_id: &KeychainSecretId) -> Result<(), PskStoreError> {
        let path = path_for(key_id, "delete")?;
        std::fs::remove_file(&path).map_err(|error| backend("delete", key_id, error))
    }

    fn path_for(
        key_id: &KeychainSecretId,
        operation: &'static str,
    ) -> Result<PathBuf, PskStoreError> {
        let home = std::env::var_os("HOME").ok_or_else(|| PskStoreError::Backend {
            operation,
            key_id: key_id.clone(),
            message: "HOME is not set for desktop-dev PSK store".to_owned(),
        })?;
        Ok(Path::new(&home)
            .join(".tyde")
            .join(format!("{KEYCHAIN_SERVICE}-dev"))
            .join(safe_file_name(key_id, operation)?))
    }

    fn backend(
        operation: &'static str,
        key_id: &KeychainSecretId,
        error: io::Error,
    ) -> PskStoreError {
        PskStoreError::Backend {
            operation,
            key_id: key_id.clone(),
            message: error.to_string(),
        }
    }
}

#[cfg(any(
    target_os = "android",
    all(
        not(target_os = "ios"),
        not(target_os = "android"),
        not(debug_assertions)
    )
))]
mod platform {
    use super::{KEYCHAIN_SERVICE, KeychainSecretId, PskStoreError};

    pub(super) fn store_secret(
        key_id: &KeychainSecretId,
        _bytes: &[u8],
    ) -> Result<(), PskStoreError> {
        Err(unsupported("store", key_id))
    }

    pub(super) fn load_secret(key_id: &KeychainSecretId) -> Result<Vec<u8>, PskStoreError> {
        Err(unsupported("load", key_id))
    }

    pub(super) fn delete_secret(key_id: &KeychainSecretId) -> Result<(), PskStoreError> {
        Err(unsupported("delete", key_id))
    }

    fn unsupported(operation: &'static str, key_id: &KeychainSecretId) -> PskStoreError {
        PskStoreError::Backend {
            operation,
            key_id: key_id.clone(),
            message: format!(
                "{KEYCHAIN_SERVICE} secure storage is unsupported on this platform; mobile pairing requires iOS Keychain support"
            ),
        }
    }
}

#[cfg(all(not(target_os = "ios"), not(target_os = "android"), debug_assertions))]
fn safe_file_name(
    key_id: &KeychainSecretId,
    operation: &'static str,
) -> Result<String, PskStoreError> {
    let raw = key_id_str(key_id);
    if !raw.is_empty()
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Ok(raw.to_owned());
    }
    Err(PskStoreError::Backend {
        operation,
        key_id: key_id.clone(),
        message:
            "keychain secret id contains characters that are invalid for desktop-dev file storage"
                .to_owned(),
    })
}

#[cfg(any(
    target_os = "ios",
    all(not(target_os = "ios"), not(target_os = "android"), debug_assertions)
))]
fn key_id_str(key_id: &KeychainSecretId) -> &str {
    &key_id.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn generated_ids_are_opaque_psk_handles() {
        let id = new_keychain_secret_id();
        assert!(id.0.starts_with("tyde-mobile-psk-"));
    }

    #[test]
    fn psk_base64_round_trip_matches_protocol_type() {
        let psk = PreSharedKey::from_slice(&[5_u8; 32]).expect("psk");
        let encoded = URL_SAFE_NO_PAD.encode(psk.as_bytes());
        let decoded = URL_SAFE_NO_PAD.decode(encoded).expect("base64");
        assert_eq!(PreSharedKey::from_slice(&decoded).expect("psk"), psk);
    }
}
