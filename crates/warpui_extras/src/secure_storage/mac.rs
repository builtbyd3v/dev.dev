//! File-based implementation of the [`SecureStorage`] service for macOS.
//!
//! macOS previously used the login keychain (`SecKeychain` generic-password).
//! For an unsigned / ad-hoc-signed build the keychain item's ACL is keyed to
//! the app's code signature, which changes on every build and release, so macOS
//! prompted to unlock the item on every launch. Worse, a denied read was masked
//! as `NotFound`, which the auth layer reads as "no saved login" and drops to
//! re-auth. We store secrets in an owner-only (0600) file in the app state dir
//! instead — the same approach Windows and Linux already use — which removes the
//! prompt and makes login persist.
//!
//! ponytail: plaintext at rest, protected only by 0600 file perms. There is no
//! DPAPI equivalent on macOS without pulling in an encryption crate (ring/rand
//! are Linux-only here). Same posture as the existing `~/.warp/browser-mcp/token`.
//! Encrypt if a token-at-rest threat model ever demands it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;

use super::Error;

/// Implementation of the SecureStorage service using owner-only files in the
/// app state directory.
pub struct SecureStorage {
    /// The name of the service under which to store the values.
    service_name: String,
    /// The directory the keyed value files live in.
    storage_dir: PathBuf,
}

impl SecureStorage {
    pub fn new_with_path(service_name: &str, storage_dir: PathBuf) -> Self {
        Self {
            service_name: service_name.to_owned(),
            storage_dir,
        }
    }

    fn storage_file(&self, key: &str) -> PathBuf {
        let filename = format!("{}-{key}", self.service_name);
        self.storage_dir.join(filename)
    }

    /// A missing backing file means the key was never stored (`NotFound`); any
    /// other I/O error is surfaced as-is so a real failure is never silently
    /// read as "not logged in".
    fn map_io_error(err: std::io::Error) -> Error {
        if err.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound
        } else {
            Error::Unknown(err.into())
        }
    }
}

impl super::SecureStorage for SecureStorage {
    fn write_value(&self, key: &str, value: &str) -> Result<(), Error> {
        std::fs::create_dir_all(&self.storage_dir).map_err(|err| Error::Unknown(err.into()))?;
        let mut dir_permissions = std::fs::metadata(&self.storage_dir)
            .map_err(|err| Error::Unknown(err.into()))?
            .permissions();
        dir_permissions.set_mode(0o700);
        std::fs::set_permissions(&self.storage_dir, dir_permissions)
            .map_err(|err| Error::Unknown(err.into()))?;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(self.storage_file(key))
            .map_err(|err| Error::Unknown(err.into()))?;
        file.write_all(value.as_bytes())
            .map_err(|err| Error::Unknown(err.into()))?;

        // `.mode(0o600)` only applies when the file is created; re-tighten an
        // existing file so a pre-existing looser-permissioned file is fixed.
        let mut file_permissions = file
            .metadata()
            .map_err(|err| Error::Unknown(err.into()))?
            .permissions();
        file_permissions.set_mode(0o600);
        file.set_permissions(file_permissions)
            .map_err(|err| Error::Unknown(err.into()))
    }

    fn read_value(&self, key: &str) -> Result<String, Error> {
        let bytes = std::fs::read(self.storage_file(key)).map_err(Self::map_io_error)?;
        String::from_utf8(bytes).map_err(|err| Error::DecodeError(err.utf8_error()))
    }

    fn remove_value(&self, key: &str) -> Result<(), Error> {
        std::fs::remove_file(self.storage_file(key)).map_err(Self::map_io_error)
    }
}

#[cfg(test)]
#[path = "mac_tests.rs"]
mod tests;
