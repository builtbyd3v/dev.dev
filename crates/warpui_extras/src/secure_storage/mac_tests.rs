use std::os::unix::fs::PermissionsExt as _;

use super::super::{Error, SecureStorage as _};
use super::SecureStorage;

#[test]
fn write_read_remove_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = SecureStorage::new_with_path("dev.warp.Test", dir.path().to_path_buf());

    // Missing key reads as NotFound, not a hard error.
    assert!(matches!(store.read_value("User"), Err(Error::NotFound)));

    let token = "{\"id_token\":\"abc\",\"email\":\"test@warp.dev\"}";
    store.write_value("User", token).unwrap();
    assert_eq!(store.read_value("User").unwrap(), token);

    // Overwrite persists the new value.
    store.write_value("User", "replaced").unwrap();
    assert_eq!(store.read_value("User").unwrap(), "replaced");

    store.remove_value("User").unwrap();
    assert!(matches!(store.read_value("User"), Err(Error::NotFound)));
    assert!(matches!(store.remove_value("User"), Err(Error::NotFound)));
}

#[test]
fn stored_file_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = SecureStorage::new_with_path("dev.warp.Test", dir.path().to_path_buf());
    store.write_value("User", "secret").unwrap();

    let path = dir.path().join("dev.warp.Test-User");
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "token file must be owner read/write only");
}
