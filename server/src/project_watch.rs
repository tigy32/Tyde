use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc as sync;

use ignore::WalkBuilder;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{Project, ProjectPath};
use tokio::sync::{mpsc, oneshot};

use crate::project_stream::{GitAccessMode, root_is_git_repository, run_git_mode};

enum Command {
    Event(notify::Result<Event>),
    Observe(PathBuf, oneshot::Sender<notify::Result<()>>),
    Stop,
}

pub(crate) struct ProjectWatcher {
    tx: sync::Sender<Command>,
    pub(crate) observed: HashSet<ProjectPath>,
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
    }
}

impl ProjectWatcher {
    pub(crate) fn new(
        project: &Project,
        events: mpsc::UnboundedSender<notify::Result<Event>>,
    ) -> notify::Result<Self> {
        let (tx, rx) = sync::channel();
        let callback = tx.clone();
        let watcher = RecommendedWatcher::new(
            move |event| {
                let _ = callback.send(Command::Event(event));
            },
            Config::default().with_follow_symlinks(false),
        )?;
        let roots = project
            .root_paths()
            .into_iter()
            .map(|root| {
                fs::canonicalize(&root.0)
                    .map_err(|error| notify::Error::io(error).add_path(PathBuf::from(root.0)))
            })
            .collect::<notify::Result<Vec<_>>>()?;
        let mut state = WatchState {
            watcher,
            roots,
            registered: HashSet::new(),
            explicit: HashSet::new(),
            inventory: Inventory::default(),
        };
        state.reconcile()?;
        std::thread::Builder::new()
            .name("tyde-project-watch".to_owned())
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Stop => break,
                        Command::Observe(path, reply) => {
                            let result = state.observe(path);
                            let _ = reply.send(result);
                        }
                        Command::Event(Ok(event)) => match state.process(event) {
                            Ok(Some(event)) => {
                                if events.send(Ok(event)).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                let _ = events.send(Err(error));
                                break;
                            }
                        },
                        Command::Event(Err(error)) => {
                            let _ = events.send(Err(error));
                            break;
                        }
                    }
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            tx,
            observed: HashSet::new(),
        })
    }

    pub(crate) async fn observe(&mut self, path: &ProjectPath) -> notify::Result<()> {
        if self.observed.contains(path) {
            return Ok(());
        }
        let absolute = fs::canonicalize(&path.root.0)
            .map_err(notify::Error::io)?
            .join(&path.relative_path);
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Observe(absolute, reply))
            .map_err(|_| watch_error("project filesystem watcher stopped"))?;
        response
            .await
            .map_err(|_| watch_error("project filesystem watcher stopped"))??;
        self.observed.insert(path.clone());
        Ok(())
    }
}

#[derive(Default)]
struct Inventory {
    visible: HashSet<PathBuf>,
    directories: HashSet<PathBuf>,
    tracked: HashSet<PathBuf>,
    controls: HashSet<PathBuf>,
    git: HashMap<PathBuf, PathBuf>,
}

struct WatchState {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    registered: HashSet<PathBuf>,
    explicit: HashSet<PathBuf>,
    inventory: Inventory,
}

fn walk(path: &Path, depth: Option<usize>) -> ignore::Walk {
    WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(depth)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
}

fn watch_error(error: impl std::fmt::Display) -> notify::Error {
    notify::Error::generic(&error.to_string())
}

impl WatchState {
    fn register(&mut self, path: &Path, desired: &mut HashSet<PathBuf>) -> notify::Result<()> {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
            desired.insert(path.to_owned());
            if !self.registered.contains(path) {
                tracing::debug!(path = %path.display(), "registering non-recursive project filesystem watch");
                if let Err(error) = self.watcher.watch(path, RecursiveMode::NonRecursive) {
                    if matches!(&error.kind, notify::ErrorKind::Io(cause) if cause.kind() == std::io::ErrorKind::NotFound)
                    {
                        desired.remove(path);
                        return Ok(());
                    }
                    return Err(error);
                }
                self.registered.insert(path.to_owned());
            }
        }
        Ok(())
    }

    fn control(
        &mut self,
        path: PathBuf,
        next: &mut Inventory,
        desired: &mut HashSet<PathBuf>,
    ) -> notify::Result<()> {
        // Watching the parent also observes atomic replacements and newly created ignore files.
        if let Some(parent) = path.ancestors().skip(1).find(|parent| parent.is_dir()) {
            self.register(parent, desired)?;
        }
        next.controls.insert(path);
        Ok(())
    }

    fn reconcile(&mut self) -> notify::Result<()> {
        let mut next = Inventory::default();
        let mut desired = HashSet::new();
        for root in self.roots.clone() {
            self.register(&root, &mut desired)?;
            for entry in walk(&root, None) {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error)
                        if error
                            .io_error()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(watch_error(error)),
                };
                let path = entry.path().to_owned();
                if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    self.register(&path, &mut desired)?;
                    next.directories.insert(path.clone());
                    self.control(path.join(".gitignore"), &mut next, &mut desired)?;
                    self.control(path.join(".ignore"), &mut next, &mut desired)?;
                }
                next.visible.insert(path);
            }
            for parent in root.ancestors().skip(1) {
                self.control(parent.join(".gitignore"), &mut next, &mut desired)?;
                self.control(parent.join(".ignore"), &mut next, &mut desired)?;
            }
            self.control(root.join(".git"), &mut next, &mut desired)?;
            if root_is_git_repository(&root.to_string_lossy()) {
                let tracked = run_git_mode(
                    &root.to_string_lossy(),
                    &["ls-files", "-z", "--cached"],
                    GitAccessMode::ReadOnly,
                )
                .map_err(watch_error)?;
                for relative in tracked.split('\0').filter(|path| !path.is_empty()) {
                    let path = root.join(relative);
                    let parents: Vec<_> = path
                        .ancestors()
                        .skip(1)
                        .take_while(|parent| parent.starts_with(&root))
                        .collect();
                    if parents.iter().any(|parent| {
                        fs::symlink_metadata(parent).is_ok_and(|meta| meta.file_type().is_symlink())
                    }) {
                        continue;
                    }
                    for parent in parents {
                        self.register(parent, &mut desired)?;
                    }
                    next.tracked.insert(path);
                }
                for option in ["--git-dir", "--git-common-dir"] {
                    let git_dir = run_git_mode(
                        &root.to_string_lossy(),
                        &["rev-parse", "--path-format=absolute", option],
                        GitAccessMode::ReadOnly,
                    )
                    .map_err(watch_error)?;
                    let git_dir = fs::canonicalize(git_dir.trim()).map_err(notify::Error::io)?;
                    for name in ["HEAD", "index", "packed-refs", "config", "commondir"] {
                        let path = git_dir.join(name);
                        next.git.insert(path.clone(), root.join(".git/index"));
                        self.control(path, &mut next, &mut desired)?;
                    }
                    self.control(git_dir.join("info/exclude"), &mut next, &mut desired)?;
                    let refs = git_dir.join("refs");
                    self.register(&git_dir, &mut desired)?;
                    next.git.insert(refs.clone(), root.join(".git/index"));
                    if refs.is_dir() {
                        for entry in WalkBuilder::new(&refs)
                            .standard_filters(false)
                            .follow_links(false)
                            .build()
                        {
                            let entry = entry.map_err(watch_error)?;
                            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                                self.register(entry.path(), &mut desired)?;
                            }
                        }
                    }
                }
            }
        }
        let home = crate::paths::home_dir().ok();
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".config")));
        if let Some(home) = home {
            self.control(home.join(".gitconfig"), &mut next, &mut desired)?;
        }
        if let Some(config) = config {
            self.control(config.join("git/ignore"), &mut next, &mut desired)?;
            self.control(config.join("git/config"), &mut next, &mut desired)?;
        }
        for root in &self.roots.clone() {
            if let Ok(global) = run_git_mode(
                &root.to_string_lossy(),
                &["config", "--path", "--get", "core.excludesFile"],
                GitAccessMode::ReadOnly,
            ) {
                let path = PathBuf::from(global.trim());
                let path = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                self.control(path, &mut next, &mut desired)?;
            }
        }
        for path in self.explicit.clone() {
            self.register_explicit(&path, &mut desired)?;
        }
        for path in self.registered.clone().difference(&desired) {
            if let Err(error) = self.watcher.unwatch(path)
                && !matches!(error.kind, notify::ErrorKind::WatchNotFound)
                && path.exists()
            {
                return Err(error);
            }
            self.registered.remove(path);
        }
        tracing::debug!(
            directories = desired.len(),
            files = next.visible.len(),
            "reconciled ignore-aware project filesystem watches"
        );
        self.inventory = next;
        Ok(())
    }

    fn register_explicit(
        &mut self,
        path: &Path,
        desired: &mut HashSet<PathBuf>,
    ) -> notify::Result<()> {
        for parent in path.ancestors().skip(1) {
            if !self.roots.iter().any(|root| parent.starts_with(root)) {
                break;
            }
            self.register(parent, desired)?;
        }
        Ok(())
    }

    fn observe(&mut self, path: PathBuf) -> notify::Result<()> {
        let Some(parent) = path.ancestors().skip(1).find(|parent| parent.exists()) else {
            return Ok(());
        };
        let resolved = fs::canonicalize(parent).map_err(notify::Error::io)?;
        if !self.roots.iter().any(|root| resolved.starts_with(root)) {
            return Ok(());
        }
        let path = resolved.join(path.strip_prefix(parent).map_err(watch_error)?);
        self.register_explicit(&path, &mut HashSet::new())?;
        self.explicit.insert(path);
        Ok(())
    }

    fn process(&mut self, mut event: Event) -> notify::Result<Option<Event>> {
        if matches!(
            event.kind,
            EventKind::Access(_) | EventKind::Modify(notify::event::ModifyKind::Metadata(_))
        ) {
            return Ok(None);
        }
        let mut relevant = Vec::new();
        let mut reconcile = event.need_rescan();
        for path in &event.paths {
            let directory = self.registered.contains(path) || path.is_dir();
            if directory {
                for explicit in self.explicit.iter().chain(&self.inventory.tracked) {
                    if explicit != path && explicit.starts_with(path) {
                        reconcile = true;
                        relevant.push(explicit.clone());
                    }
                }
            }
            let git = self.inventory.git.iter().find(|(candidate, _)| {
                path == *candidate
                    || (candidate.file_name().is_some_and(|name| name == "refs")
                        && path.starts_with(candidate))
            });
            if let Some((_, synthetic)) = git {
                relevant.push(synthetic.clone());
                reconcile = true;
            }
            if self.inventory.controls.contains(path)
                || (directory
                    && self
                        .inventory
                        .controls
                        .iter()
                        .any(|control| control.starts_with(path)))
            {
                reconcile = true;
                relevant.extend(self.roots.iter().map(|root| root.join(".git/index")));
                if self.roots.iter().any(|root| path.starts_with(root)) {
                    relevant.push(path.clone());
                }
            }
            if self.inventory.visible.contains(path)
                || self.inventory.tracked.contains(path)
                || self.explicit.contains(path)
            {
                relevant.push(path.clone());
            }
            if self.inventory.directories.contains(path) || self.roots.contains(path) {
                reconcile = true;
            } else if let Some(parent) = path.parent()
                && self.inventory.directories.contains(parent)
                && (directory || !self.inventory.visible.contains(path))
            {
                // Only inspect siblings for a newly encountered entry, not on ordinary writes.
                for entry in walk(parent, Some(1)) {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error)
                            if error.io_error().is_some_and(|error| {
                                error.kind() == std::io::ErrorKind::NotFound
                            }) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(watch_error(error)),
                    };
                    if entry.path() == path {
                        relevant.push(path.clone());
                        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                            reconcile = true;
                        } else {
                            self.inventory.visible.insert(path.clone());
                        }
                        break;
                    }
                }
            }
        }
        if reconcile {
            if matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) {
                let removed: Vec<_> = self
                    .registered
                    .iter()
                    .filter(|registered| {
                        event.paths.iter().any(|path| registered.starts_with(path))
                    })
                    .cloned()
                    .collect();
                for path in removed {
                    if let Err(error) = self.watcher.unwatch(&path)
                        && !matches!(error.kind, notify::ErrorKind::WatchNotFound)
                        && path.exists()
                    {
                        return Err(error);
                    }
                    self.registered.remove(&path);
                }
            }
            let previous = self.inventory.visible.clone();
            self.reconcile()?;
            // Files can be created before a new directory's watch is installed.
            relevant.extend(
                self.inventory
                    .visible
                    .symmetric_difference(&previous)
                    .cloned(),
            );
        }
        if event.need_rescan() {
            relevant.extend(self.inventory.visible.iter().cloned());
            relevant.extend(self.inventory.tracked.iter().cloned());
            relevant.extend(self.explicit.iter().cloned());
            relevant.extend(self.roots.iter().map(|root| root.join(".git/index")));
        }
        relevant.sort();
        relevant.dedup();
        event.paths = relevant;
        Ok((!event.paths.is_empty()).then_some(event))
    }
}
