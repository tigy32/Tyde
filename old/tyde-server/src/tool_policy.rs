use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "mode", content = "tools")]
pub enum ToolPolicy {
    #[default]
    Unrestricted,
    AllowList(Vec<String>),
    DenyList(Vec<String>),
}
