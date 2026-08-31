pub use host_config::{HostDisconnectedEvent, HostErrorEvent, HostLineEvent, HostWarningEvent};

pub const HOST_LINE_EVENT: &str = "tyde://host-line";
pub const HOST_DISCONNECTED_EVENT: &str = "tyde://host-disconnected";
pub const HOST_ERROR_EVENT: &str = "tyde://host-error";
pub const HOST_WARNING_EVENT: &str = "tyde://host-warning";
pub const HOST_LIFECYCLE_EVENT: &str = "tyde://host-lifecycle";
