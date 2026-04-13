use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct FileChangedPayload {
    pub path: String,
}

pub struct FileWatchManager {
    watcher: RecommendedWatcher,
    watched_paths: HashSet<PathBuf>,
    watched_dir: Option<PathBuf>,
}

impl FileWatchManager {
    pub fn new<F>(on_change: F) -> Result<Self, String>
    where
        F: Fn(FileChangedPayload) + Send + Sync + 'static,
    {
        let on_change = Arc::new(on_change);
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| match result {
                Ok(event) => {
                    if !should_emit_event(&event.kind) {
                        return;
                    }

                    let mut emitted_paths = HashSet::new();
                    for path in event.paths {
                        let path_string = path.to_string_lossy().to_string();
                        if path_string.is_empty() || !emitted_paths.insert(path_string.clone()) {
                            continue;
                        }
                        on_change(FileChangedPayload { path: path_string });
                    }
                }
                Err(err) => {
                    tracing::warn!("File watcher event error: {err}");
                }
            },
            Config::default(),
        )
        .map_err(|err| format!("Failed to initialize file watcher: {err}"))?;

        Ok(Self {
            watcher,
            watched_paths: HashSet::new(),
            watched_dir: None,
        })
    }

    pub fn sync_paths(&mut self, paths: &[String]) {
        let desired_paths: HashSet<PathBuf> = paths
            .iter()
            .map(|path| PathBuf::from(path.as_str()))
            .collect();

        let paths_to_remove: Vec<PathBuf> = self
            .watched_paths
            .difference(&desired_paths)
            .cloned()
            .collect();
        for path in paths_to_remove {
            if let Err(err) = self.watcher.unwatch(&path) {
                tracing::warn!("Failed to unwatch {}: {err}", path.display());
            }
            self.watched_paths.remove(&path);
        }

        let paths_to_add: Vec<PathBuf> = desired_paths
            .difference(&self.watched_paths)
            .cloned()
            .collect();
        for path in paths_to_add {
            if let Err(err) = self.watcher.watch(&path, RecursiveMode::NonRecursive) {
                tracing::warn!("Failed to watch {}: {err}", path.display());
                continue;
            }
            self.watched_paths.insert(path);
        }
    }

    pub fn watch_dir(&mut self, path: &str) {
        let dir = PathBuf::from(path);
        if self.watched_dir.as_ref() == Some(&dir) {
            return;
        }
        self.unwatch_dir();
        if let Err(err) = self.watcher.watch(&dir, RecursiveMode::Recursive) {
            tracing::warn!("Failed to watch directory {}: {err}", dir.display());
            return;
        }
        self.watched_dir = Some(dir);
    }

    pub fn unwatch_dir(&mut self) {
        if let Some(dir) = self.watched_dir.take() {
            if let Err(err) = self.watcher.unwatch(&dir) {
                tracing::warn!("Failed to unwatch directory {}: {err}", dir.display());
            }
        }
    }
}

fn should_emit_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Other
    )
}
