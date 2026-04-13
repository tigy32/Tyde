use std::path::Path;
use std::sync::{LazyLock, Mutex, MutexGuard};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) struct TempHomeGuard {
    _lock: MutexGuard<'static, ()>,
    old_home: Option<String>,
    old_userprofile: Option<String>,
}

impl TempHomeGuard {
    pub(crate) fn new(path: &Path) -> Self {
        let lock = ENV_LOCK.lock().expect("env lock poisoned");
        let old_home = std::env::var("HOME").ok();
        let old_userprofile = std::env::var("USERPROFILE").ok();
        std::env::set_var("HOME", path);
        std::env::set_var("USERPROFILE", path);
        Self {
            _lock: lock,
            old_home,
            old_userprofile,
        }
    }
}

impl Drop for TempHomeGuard {
    fn drop(&mut self) {
        if let Some(home) = &self.old_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(userprofile) = &self.old_userprofile {
            std::env::set_var("USERPROFILE", userprofile);
        } else {
            std::env::remove_var("USERPROFILE");
        }
    }
}
