use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    FrameKind, HostAbsPath, HostBrowseEntriesPayload, HostBrowseEntry, HostBrowseEntryError,
    HostBrowseErrorCode, HostBrowseErrorPayload, HostBrowseOpenedPayload, HostPlatform,
    ProjectFileKind,
};
use tokio::fs;

use crate::stream::Stream;

const MAX_ENTRIES: usize = 5000;

pub(crate) fn host_platform() -> HostPlatform {
    if cfg!(target_os = "macos") {
        HostPlatform::Macos
    } else if cfg!(target_os = "linux") {
        HostPlatform::Linux
    } else if cfg!(target_os = "windows") {
        HostPlatform::Windows
    } else {
        HostPlatform::Other
    }
}

pub(crate) fn home_dir() -> HostAbsPath {
    let path = std::env::var("HOME").unwrap_or_else(|err| {
        panic!("cannot open browse stream: HOME is not set in this server process: {err}")
    });
    assert!(
        Path::new(&path).is_absolute(),
        "HOME must be an absolute path, got {path}"
    );
    HostAbsPath(path)
}

pub(crate) fn opened_payload(initial: &HostAbsPath) -> HostBrowseOpenedPayload {
    HostBrowseOpenedPayload {
        home: initial.clone(),
        root: HostAbsPath("/".to_owned()),
        separator: '/',
        platform: host_platform(),
    }
}

pub(crate) async fn emit_opened(stream: &Stream, payload: &HostBrowseOpenedPayload) {
    let value =
        serde_json::to_value(payload).expect("failed to serialize HostBrowseOpened payload");
    let _ = stream.send_value(FrameKind::HostBrowseOpened, value).await;
}

pub(crate) async fn emit_entries(stream: &Stream, payload: &HostBrowseEntriesPayload) {
    let value =
        serde_json::to_value(payload).expect("failed to serialize HostBrowseEntries payload");
    let _ = stream.send_value(FrameKind::HostBrowseEntries, value).await;
}

pub(crate) async fn emit_error(stream: &Stream, payload: &HostBrowseErrorPayload) {
    let value = serde_json::to_value(payload).expect("failed to serialize HostBrowseError payload");
    let _ = stream.send_value(FrameKind::HostBrowseError, value).await;
}

pub(crate) async fn list_dir(
    path: &HostAbsPath,
    include_hidden: bool,
) -> Result<HostBrowseEntriesPayload, HostBrowseErrorPayload> {
    let fs_path = PathBuf::from(&path.0);

    let meta = match fs::symlink_metadata(&fs_path).await {
        Ok(meta) => meta,
        Err(err) => return Err(map_target_error(path, err)),
    };
    if !meta.is_dir() {
        return Err(HostBrowseErrorPayload {
            path: path.clone(),
            code: HostBrowseErrorCode::NotADirectory,
            message: format!("not a directory: {}", path.0),
        });
    }

    let mut read_dir = match fs::read_dir(&fs_path).await {
        Ok(read_dir) => read_dir,
        Err(err) => return Err(map_target_error(path, err)),
    };

    let mut entries: Vec<HostBrowseEntry> = Vec::new();
    loop {
        let next = match read_dir.next_entry().await {
            Ok(next) => next,
            Err(err) => return Err(map_target_error(path, err)),
        };
        let Some(dir_entry) = next else {
            break;
        };

        let file_name = dir_entry.file_name().to_string_lossy().to_string();
        let is_hidden = file_name.starts_with('.');
        if !include_hidden && is_hidden {
            continue;
        }

        let entry = match build_entry(&dir_entry, file_name.clone(), is_hidden).await {
            Ok(entry) => entry,
            Err(entry_error) => HostBrowseEntry {
                name: file_name,
                kind: ProjectFileKind::File,
                size: None,
                mtime_ms: None,
                is_hidden,
                symlink_target: None,
                entry_error: Some(entry_error),
            },
        };

        entries.push(entry);

        if entries.len() > MAX_ENTRIES {
            return Err(HostBrowseErrorPayload {
                path: path.clone(),
                code: HostBrowseErrorCode::TooLarge,
                message: format!(
                    "directory has more than {} entries; refusing to enumerate",
                    MAX_ENTRIES
                ),
            });
        }
    }

    entries.sort_by(|a, b| {
        let a_dir = matches!(a.kind, ProjectFileKind::Directory);
        let b_dir = matches!(b.kind, ProjectFileKind::Directory);
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let parent = fs_path.parent().and_then(|p| {
        let s = p.to_string_lossy().to_string();
        if s.is_empty() {
            None
        } else {
            Some(HostAbsPath(s))
        }
    });

    Ok(HostBrowseEntriesPayload {
        path: path.clone(),
        parent,
        entries,
    })
}

async fn build_entry(
    dir_entry: &tokio::fs::DirEntry,
    name: String,
    is_hidden: bool,
) -> Result<HostBrowseEntry, HostBrowseEntryError> {
    let symlink_meta = dir_entry
        .metadata()
        .await
        .map_err(map_entry_error_from_io)?;

    if symlink_meta.file_type().is_symlink() {
        let target = fs::read_link(dir_entry.path())
            .await
            .ok()
            .map(|p| HostAbsPath(p.to_string_lossy().to_string()));
        let resolved = fs::metadata(dir_entry.path()).await;
        let kind = match &resolved {
            Ok(meta) if meta.is_dir() => ProjectFileKind::Symlink,
            Ok(_) => ProjectFileKind::Symlink,
            Err(_) => ProjectFileKind::Symlink,
        };
        let size = resolved.as_ref().ok().map(|m| m.len());
        let mtime_ms = resolved
            .as_ref()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(system_time_to_ms);
        let entry_error = if resolved.is_err() {
            Some(HostBrowseEntryError::BrokenSymlink)
        } else {
            None
        };
        return Ok(HostBrowseEntry {
            name,
            kind,
            size,
            mtime_ms,
            is_hidden,
            symlink_target: target,
            entry_error,
        });
    }

    let kind = if symlink_meta.is_dir() {
        ProjectFileKind::Directory
    } else {
        ProjectFileKind::File
    };
    let size = if matches!(kind, ProjectFileKind::File) {
        Some(symlink_meta.len())
    } else {
        None
    };
    let mtime_ms = symlink_meta.modified().ok().and_then(system_time_to_ms);

    Ok(HostBrowseEntry {
        name,
        kind,
        size,
        mtime_ms,
        is_hidden,
        symlink_target: None,
        entry_error: None,
    })
}

fn map_entry_error_from_io(err: io::Error) -> HostBrowseEntryError {
    match err.kind() {
        io::ErrorKind::PermissionDenied => HostBrowseEntryError::PermissionDenied,
        _ => HostBrowseEntryError::StatFailed,
    }
}

fn map_target_error(path: &HostAbsPath, err: io::Error) -> HostBrowseErrorPayload {
    let code = match err.kind() {
        io::ErrorKind::NotFound => HostBrowseErrorCode::NotFound,
        io::ErrorKind::PermissionDenied => HostBrowseErrorCode::PermissionDenied,
        _ => HostBrowseErrorCode::Internal,
    };
    HostBrowseErrorPayload {
        path: path.clone(),
        code,
        message: err.to_string(),
    }
}

fn system_time_to_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}
