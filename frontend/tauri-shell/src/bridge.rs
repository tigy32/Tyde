use serde::Serialize;

pub const HOST_LINE_EVENT: &str = "tyde://host-line";
pub const HOST_DISCONNECTED_EVENT: &str = "tyde://host-disconnected";
pub const HOST_ERROR_EVENT: &str = "tyde://host-error";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostLineEvent {
    pub host_id: String,
    pub line: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDisconnectedEvent {
    pub host_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostErrorEvent {
    pub host_id: String,
    pub message: String,
}
