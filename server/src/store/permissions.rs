use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
pub(crate) fn atomic_write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("store path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "failed to create store directory {}: {err}",
            parent.display()
        )
    })?;

    let file_name = path
        .file_name()
        .ok_or_else(|| format!("store path has no file name: {}", path.display()))?
        .to_string_lossy();
    let tmp_path = path.with_file_name(format!(".{file_name}.tmp"));

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp_path)
        .map_err(|err| {
            format!(
                "failed to create temp store file {}: {err}",
                tmp_path.display()
            )
        })?;
    file.write_all(bytes).map_err(|err| {
        format!(
            "failed to write temp store file {}: {err}",
            tmp_path.display()
        )
    })?;
    file.sync_all().map_err(|err| {
        format!(
            "failed to sync temp store file {}: {err}",
            tmp_path.display()
        )
    })?;
    drop(file);

    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
        format!(
            "failed to set owner-only permissions on temp store file {}: {err}",
            tmp_path.display()
        )
    })?;
    std::fs::rename(&tmp_path, path).map_err(|err| {
        format!(
            "failed to atomically replace store file {}: {err}",
            path.display()
        )
    })?;
    enforce_owner_only_file(path)
}

#[cfg(not(unix))]
pub(crate) fn atomic_write_owner_only(_path: &Path, _bytes: &[u8]) -> Result<(), String> {
    Err("owner-only file permissions are unsupported on this platform".to_owned())
}

#[cfg(unix)]
pub(crate) fn enforce_owner_only_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| format!("failed to stat store file {}: {err}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("store path {} is not a file", path.display()));
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            format!(
                "failed to enforce owner-only permissions on store file {}: {err}",
                path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn enforce_owner_only_file(_path: &Path) -> Result<(), String> {
    Err("owner-only file permissions are unsupported on this platform".to_owned())
}
