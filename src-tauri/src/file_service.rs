use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::remote::{parse_remote_path, run_ssh_command, to_remote_uri};

#[derive(Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub truncated: bool,
}

pub async fn list_directory(path: &str, show_hidden: bool) -> Result<Vec<FileEntry>, String> {
    if let Some(remote) = parse_remote_path(path) {
        return list_directory_remote(&remote.host, &remote.path, show_hidden).await;
    }

    let mut reader = fs::read_dir(path).await.map_err(|e| format!("{e:?}"))?;
    let mut entries = Vec::new();

    while let Some(entry) = reader.next_entry().await.map_err(|e| format!("{e:?}"))? {
        let name = entry.file_name().to_string_lossy().to_string();

        if !show_hidden
            && (name.starts_with('.')
                || ["node_modules", "target", ".git"].contains(&name.as_str()))
        {
            continue;
        }

        let metadata = entry.metadata().await.map_err(|e| format!("{e:?}"))?;
        let is_directory = metadata.is_dir();
        let size = if is_directory {
            None
        } else {
            Some(metadata.len())
        };

        entries.push(FileEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_directory,
            size,
        });
    }

    entries.sort_by(|a, b| match (a.is_directory, b.is_directory) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

pub async fn read_file_content(path: &str) -> Result<FileContent, String> {
    if let Some(remote) = parse_remote_path(path) {
        return read_file_content_remote(path, &remote.host, &remote.path).await;
    }

    let metadata = fs::metadata(path).await.map_err(|e| format!("{e:?}"))?;
    let size = metadata.len();

    let bytes = fs::read(path).await.map_err(|e| format!("{e:?}"))?;

    let check_len = bytes.len().min(512);
    if bytes[..check_len].contains(&0x00) {
        return Ok(FileContent {
            path: path.to_string(),
            content: "Binary file".to_string(),
            size,
            truncated: false,
        });
    }

    let truncated = size > 1_048_576;
    let usable = if truncated {
        &bytes[..1_048_576_usize]
    } else {
        &bytes
    };

    let content = String::from_utf8_lossy(usable).to_string();

    Ok(FileContent {
        path: path.to_string(),
        content,
        size,
        truncated,
    })
}

#[derive(Deserialize)]
struct RemoteFileContentPayload {
    content: String,
    size: u64,
    truncated: bool,
}

async fn list_directory_remote(
    host: &str,
    path: &str,
    show_hidden: bool,
) -> Result<Vec<FileEntry>, String> {
    let script = r#"
import json, os, sys
root = sys.argv[1]
show_hidden = sys.argv[2] == "1"
skip = {"node_modules", "target", ".git"}
entries = []
with os.scandir(root) as it:
    for entry in it:
        name = entry.name
        if not show_hidden and (name.startswith('.') or name in skip):
            continue
        try:
            st = entry.stat(follow_symlinks=False)
            is_dir = entry.is_dir(follow_symlinks=False)
        except OSError:
            continue
        entries.append({
            "name": name,
            "path": os.path.join(root, name),
            "is_directory": is_dir,
            "size": None if is_dir else st.st_size
        })
entries.sort(key=lambda e: (0 if e["is_directory"] else 1, e["name"].lower()))
print(json.dumps(entries))
"#;

    let args = vec![
        "python3".to_string(),
        "-c".to_string(),
        script.to_string(),
        path.to_string(),
        if show_hidden {
            "1".to_string()
        } else {
            "0".to_string()
        },
    ];

    let output = run_ssh_command(host, &args).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh list directory failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|e| format!("{e:?}"))?;
    let mut entries: Vec<FileEntry> = serde_json::from_str(&stdout)
        .map_err(|e| format!("Failed to decode remote directory listing: {e}"))?;

    for entry in &mut entries {
        entry.path = to_remote_uri(host, &entry.path);
    }

    Ok(entries)
}

async fn read_file_content_remote(
    original_uri: &str,
    host: &str,
    path: &str,
) -> Result<FileContent, String> {
    let script = r#"
import json, os, sys
path = sys.argv[1]
max_bytes = 1024 * 1024

size = os.path.getsize(path)
with open(path, "rb") as f:
    data = f.read(max_bytes + 1)

if b"\x00" in data[:512]:
    print(json.dumps({
        "content": "Binary file",
        "size": size,
        "truncated": False
    }))
else:
    truncated = len(data) > max_bytes or size > max_bytes
    usable = data[:max_bytes]
    print(json.dumps({
        "content": usable.decode("utf-8", errors="replace"),
        "size": size,
        "truncated": truncated
    }))
"#;

    let args = vec![
        "python3".to_string(),
        "-c".to_string(),
        script.to_string(),
        path.to_string(),
    ];

    let output = run_ssh_command(host, &args).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ssh read file failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|e| format!("{e:?}"))?;
    let payload: RemoteFileContentPayload = serde_json::from_str(&stdout)
        .map_err(|e| format!("Failed to decode remote file content: {e}"))?;

    Ok(FileContent {
        path: original_uri.to_string(),
        content: payload.content,
        size: payload.size,
        truncated: payload.truncated,
    })
}
