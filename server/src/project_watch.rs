use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc as sync;
use std::sync::{Arc, Mutex};

use ignore::WalkBuilder;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{Project, ProjectPath};
use tokio::sync::{mpsc, oneshot};

use crate::project_stream::{GitAccessMode, root_is_git_repository, run_git_mode};

enum Command {
    Event(Event),
    Rescan,
    Failure(String),
    Observe(PathBuf, oneshot::Sender<notify::Result<()>>),
    Stop,
}

#[derive(Clone)]
struct EventSink {
    tx: sync::Sender<Command>,
    pending: Arc<AtomicUsize>,
    rescan: Arc<AtomicBool>,
    #[cfg(feature = "test-support")]
    roots: Vec<PathBuf>,
}

impl EventSink {
    fn send(&self, event: &Event) {
        if self.pending.fetch_add(1, Ordering::Relaxed) < 128 {
            if self.tx.send(Command::Event(event.clone())).is_err() {
                self.pending.fetch_sub(1, Ordering::Relaxed);
            }
        } else {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            if !self.rescan.swap(true, Ordering::Relaxed) {
                tracing::warn!("project watch queue full; scheduling catch-up rescan");
                let _ = self.tx.send(Command::Rescan);
                #[cfg(feature = "test-support")]
                for root in &self.roots {
                    crate::project_stream::scan_test_support::run(
                        root,
                        crate::project_stream::scan_test_support::ScanPoint::WatcherOverflow,
                    );
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct SharedProjectWatcher {
    inner: Arc<Mutex<SharedState>>,
}

#[derive(Default)]
struct SharedState {
    watcher: Option<RecommendedWatcher>,
    routes: Arc<Mutex<Routes>>,
    next_id: u64,
}

#[derive(Default)]
struct Routes {
    generation: u64,
    failed: bool,
    owners: BTreeMap<PathBuf, HashSet<u64>>,
    subscribers: HashMap<u64, EventSink>,
    invalidated: HashSet<PathBuf>,
}

impl Routes {
    fn dispatch(&mut self, generation: u64, result: notify::Result<Event>) {
        if self.generation != generation || self.failed {
            return;
        }
        let event = match result {
            Ok(event) => event,
            Err(error) => {
                self.failed = true;
                tracing::warn!(%error, "shared project filesystem watcher failed");
                for sink in self.subscribers.values() {
                    let _ = sink.tx.send(Command::Failure(error.to_string()));
                }
                return;
            }
        };
        if !event.need_rescan()
            && matches!(
                event.kind,
                EventKind::Access(_) | EventKind::Modify(notify::event::ModifyKind::Metadata(_))
            )
        {
            return;
        }
        #[cfg(feature = "test-support")]
        if event.paths.iter().any(|path| {
            crate::project_stream::scan_test_support::run(
                path,
                crate::project_stream::scan_test_support::ScanPoint::WatcherFailure,
            )
        }) {
            self.dispatch(
                generation,
                Err(watch_error("injected native filesystem watcher failure")),
            );
            return;
        }
        #[cfg(feature = "test-support")]
        for path in &event.paths {
            crate::project_stream::scan_test_support::run(
                path,
                crate::project_stream::scan_test_support::ScanPoint::WatcherDispatch,
            );
        }
        let mut recipients = HashSet::new();
        if event.need_rescan() {
            tracing::warn!(
                projects = self.subscribers.len(),
                "native project watcher lost events; scheduling catch-up rescans"
            );
            #[cfg(feature = "test-support")]
            for sink in self.subscribers.values() {
                for root in &sink.roots {
                    crate::project_stream::scan_test_support::run(
                        root,
                        crate::project_stream::scan_test_support::ScanPoint::WatcherKernelRescan,
                    );
                }
            }
            // Only inotify can retain dead kernel descriptors after lost events.
            // FSEvents path subscriptions need a rescan, not a stream restart.
            if cfg!(target_os = "linux") {
                self.invalidated.extend(self.owners.keys().cloned());
            }
            recipients.extend(self.subscribers.keys().copied());
        } else {
            for path in &event.paths {
                for directory in std::iter::once(path.as_path()).chain(path.parent()) {
                    if let Some(owners) = self.owners.get(directory) {
                        recipients.extend(owners);
                    }
                }
                if matches!(
                    event.kind,
                    EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
                ) {
                    for (directory, owners) in self
                        .owners
                        .range(path.clone()..)
                        .take_while(|(directory, _)| directory.starts_with(path))
                    {
                        self.invalidated.insert(directory.clone());
                        recipients.extend(owners);
                    }
                }
            }
        }
        for owner in recipients {
            if let Some(sink) = self.subscribers.get(&owner) {
                sink.send(&event);
            }
        }
    }
}

impl SharedProjectWatcher {
    fn for_watcher(&self) -> Self {
        // FSEvents restarts from "now" on registration changes. A replacement
        // must not disturb the old project's watcher while it is still live.
        if cfg!(target_os = "linux") {
            self.clone()
        } else {
            Self::default()
        }
    }

    fn subscribe(&self, project: &Project, sink: EventSink) -> notify::Result<WatchLease> {
        let mut state = self.inner.lock().unwrap();
        let failed = state.routes.lock().unwrap().failed;
        if failed {
            let mut routes = state.routes.lock().unwrap();
            routes.generation += 1;
            routes.subscribers.clear();
            routes.owners.clear();
            routes.invalidated.clear();
            routes.failed = false;
            drop(routes);
            state.watcher = None;
        }
        if state.watcher.is_none() {
            let routes = Arc::clone(&state.routes);
            let generation = routes.lock().unwrap().generation;
            #[cfg(feature = "test-support")]
            for root in project.root_paths() {
                crate::project_stream::scan_test_support::run(
                    Path::new(&root.0),
                    crate::project_stream::scan_test_support::ScanPoint::WatcherInitialize,
                );
            }
            tracing::debug!(
                roots = project.root_paths().len(),
                "creating shared project filesystem watcher"
            );
            let watcher = RecommendedWatcher::new(
                move |event| routes.lock().unwrap().dispatch(generation, event),
                Config::default().with_follow_symlinks(false),
            );
            #[cfg(feature = "test-support")]
            for root in project.root_paths() {
                crate::project_stream::scan_test_support::run(
                    Path::new(&root.0),
                    crate::project_stream::scan_test_support::ScanPoint::WatcherInitialized,
                );
            }
            #[cfg(target_os = "linux")]
            let watcher = watcher.map_err(watcher_creation_error);
            state.watcher = Some(watcher?);
        }
        state.next_id += 1;
        let id = state.next_id;
        state.routes.lock().unwrap().subscribers.insert(id, sink);
        Ok(WatchLease {
            shared: self.clone(),
            id,
            registered: HashSet::new(),
        })
    }
}

struct WatchLease {
    shared: SharedProjectWatcher,
    id: u64,
    registered: HashSet<PathBuf>,
}

impl WatchLease {
    fn invalidated(&self) -> HashSet<PathBuf> {
        let state = self.shared.inner.lock().unwrap();
        let routes = state.routes.lock().unwrap();
        self.registered
            .intersection(&routes.invalidated)
            .cloned()
            .collect()
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        let mut state = self.shared.inner.lock().unwrap();
        let mut routes = state.routes.lock().unwrap();
        if routes.failed || !routes.subscribers.contains_key(&self.id) {
            return Err(watch_error("shared project filesystem watcher restarting"));
        }
        let invalidated = routes.invalidated.remove(path);
        let owners = routes.owners.entry(path.to_owned()).or_default();
        let register = owners.is_empty() || invalidated;
        owners.insert(self.id);
        // The native watcher invokes its callback while servicing watch commands.
        // Never hold the routing lock across a synchronous native watch/unwatch.
        drop(routes);
        if register {
            let watcher = state.watcher.as_mut().expect("active watch lease");
            if invalidated {
                let _ = watcher.unwatch(path);
            }
            if let Err(error) = watcher.watch(path, mode) {
                let mut routes = state.routes.lock().unwrap();
                if let Some(owners) = routes.owners.get_mut(path) {
                    owners.remove(&self.id);
                    if owners.is_empty() {
                        routes.owners.remove(path);
                    } else {
                        routes.invalidated.insert(path.to_owned());
                    }
                }
                return Err(error);
            }
        }
        self.registered.insert(path.to_owned());
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.registered.remove(path);
        let mut state = self.shared.inner.lock().unwrap();
        let mut routes = state.routes.lock().unwrap();
        let Some(owners) = routes.owners.get_mut(path) else {
            return Ok(());
        };
        if !owners.remove(&self.id) || !owners.is_empty() {
            return Ok(());
        }
        routes.owners.remove(path);
        routes.invalidated.remove(path);
        drop(routes);
        if let Some(watcher) = state.watcher.as_mut()
            && let Err(error) = watcher.unwatch(path)
        {
            tracing::debug!(path = %path.display(), %error, "native project watch removal failed");
            #[cfg(feature = "test-support")]
            crate::project_stream::scan_test_support::run(
                path,
                crate::project_stream::scan_test_support::ScanPoint::WatcherUnwatchFailed,
            );
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for WatchLease {
    fn drop(&mut self) {
        let mut state = self.shared.inner.lock().unwrap();
        let mut routes = state.routes.lock().unwrap();
        routes.subscribers.remove(&self.id);
        if routes.subscribers.is_empty() {
            routes.generation += 1;
            routes.owners.clear();
            routes.invalidated.clear();
            routes.failed = false;
            drop(routes);
            let watcher = state.watcher.take();
            drop(state);
            drop(watcher);
            tracing::debug!("released last shared project filesystem watcher lease");
            return;
        }
        drop(routes);
        drop(state);
        for path in self.registered.clone() {
            if let Err(error) = self.unwatch(&path)
                && !matches!(error.kind, notify::ErrorKind::WatchNotFound)
            {
                tracing::warn!(%error, "failed to release project filesystem watch");
            }
        }
    }
}

pub(crate) struct ProjectWatcher {
    tx: sync::Sender<Command>,
    pub(crate) shared: SharedProjectWatcher,
    pub(crate) observed: HashSet<ProjectPath>,
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
    }
}

impl ProjectWatcher {
    pub(crate) fn new(
        shared: SharedProjectWatcher,
        project: &Project,
        events: mpsc::UnboundedSender<notify::Result<Event>>,
    ) -> notify::Result<Self> {
        let shared = shared.for_watcher();
        let (tx, rx) = sync::channel();
        let sink = EventSink {
            tx: tx.clone(),
            pending: Arc::default(),
            rescan: Arc::default(),
            #[cfg(feature = "test-support")]
            roots: project
                .root_paths()
                .into_iter()
                .map(|root| PathBuf::from(root.0))
                .collect(),
        };
        let watcher = shared.subscribe(project, sink.clone())?;
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
        #[cfg(feature = "test-support")]
        for root in &state.roots {
            crate::project_stream::scan_test_support::run(
                root,
                crate::project_stream::scan_test_support::ScanPoint::WatcherReady,
            );
        }
        std::thread::Builder::new()
            .name("tyde-project-watch".to_owned())
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    let event = match command {
                        Command::Stop => break,
                        Command::Observe(path, reply) => {
                            let result = state.observe(path);
                            let _ = reply.send(result);
                            continue;
                        }
                        Command::Event(event) => {
                            sink.pending.fetch_sub(1, Ordering::Relaxed);
                            event
                        }
                        Command::Rescan => {
                            sink.rescan.store(false, Ordering::Relaxed);
                            Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
                        }
                        Command::Failure(error) => {
                            let _ = events.send(Err(watch_error(error)));
                            break;
                        }
                    };
                    #[cfg(feature = "test-support")]
                    for root in &state.roots {
                        crate::project_stream::scan_test_support::run(
                            root,
                            crate::project_stream::scan_test_support::ScanPoint::WatcherProcess,
                        );
                    }
                    match state.process(event) {
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
                    }
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            tx,
            shared,
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

#[cfg(target_os = "linux")]
fn watcher_creation_error(error: notify::Error) -> notify::Error {
    // Linux uses EMFILE for both inotify-instance and process-descriptor exhaustion.
    if matches!(&error.kind, notify::ErrorKind::Io(io) if io.raw_os_error() == Some(rustix::io::Errno::MFILE.raw_os_error()))
    {
        return notify::Error::generic(&format!(
            "{error}. Linux watcher creation may have reached either the per-user inotify instance limit (fs.inotify.max_user_instances), shared with IDEs and other processes, or this server's open-file limit (RLIMIT_NOFILE). Check `sysctl fs.inotify.max_user_instances` on the server. If the instance limit is exhausted, ask an administrator to increase it; raising the open-file limit alone will not help."
        )).set_paths(error.paths);
    }
    error
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
    watcher: WatchLease,
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
        if !event.need_rescan()
            && matches!(
                event.kind,
                EventKind::Access(_) | EventKind::Modify(notify::event::ModifyKind::Metadata(_))
            )
        {
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
            let removes_paths = matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
            );
            if event.need_rescan() || removes_paths {
                let invalidated = if event.need_rescan() {
                    self.watcher.invalidated()
                } else {
                    HashSet::new()
                };
                let removed: Vec<_> = self
                    .registered
                    .iter()
                    .filter(|registered| {
                        (event.need_rescan() && invalidated.contains(*registered))
                            || (removes_paths
                                && event.paths.iter().any(|path| registered.starts_with(path)))
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
