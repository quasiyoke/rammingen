use base64::{prelude::BASE64_URL_SAFE_NO_PAD, Engine};
use core::fmt;
use derivative::Derivative;
use generic_array::GenericArray;
use rammingen_protocol::ArchivePath;
use serde::de::Error;
use serde::Deserialize;
use typenum::U64;

use crate::path::SanitizedLocalPath;
use crate::rules::Rule;

#[derive(Debug, Clone, Deserialize)]
pub struct MountPoint {
    pub local_path: SanitizedLocalPath,
    pub archive_path: ArchivePath,
    pub exclude: Vec<Rule>,
}

#[derive(Clone)]
pub struct EncryptionKey(pub GenericArray<u8, U64>);

impl fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptionKey").finish()
    }
}

impl<'de> Deserialize<'de> for EncryptionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let string = String::deserialize(deserializer)?;
        let binary = BASE64_URL_SAFE_NO_PAD
            .decode(string)
            .map_err(D::Error::custom)?;
        let array = <[u8; 64]>::try_from(binary).map_err(|vec| {
            D::Error::custom(format!(
                "invalid encryption key length, expected 64, got {}",
                vec.len()
            ))
        })?;
        Ok(Self(array.into()))
    }
}

#[derive(Derivative, Clone, Deserialize)]
#[derivative(Debug)]
pub struct Config {
    pub always_exclude: Vec<Rule>,
    pub mount_points: Vec<MountPoint>,
    pub encryption_key: EncryptionKey,
    pub server_url: String,
    #[derivative(Debug = "ignore")]
    pub token: String,
    #[derivative(Debug = "ignore")]
    pub salt: String,
}
