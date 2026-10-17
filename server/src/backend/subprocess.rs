use serde::{Deserialize, Serialize};

/// The canonical lease signals the group and retains its native reaper on drop.
pub(crate) fn reap_group_child(child: AsyncGroupChild) {
    drop(child);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub media_type: String,
    pub name: String,
    pub size: u64,
}

#[cfg(unix)]
pub use tyde_process_env::launch::exec_helper_entry;
#[cfg(all(feature = "test-support", target_os = "linux"))]
pub use tyde_process_env::launch::test_support;
pub(crate) use tyde_process_env::launch::{
    AsyncCommandGroup, AsyncGroupChild, GroupCommand, ProcessOwner, ProcessStdio, command, spawn,
};
