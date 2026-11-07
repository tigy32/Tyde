//! Presentation and platform I/O only. Host protocol and agent state stay in Wasm.

use serde::{Deserialize, Serialize};

macro_rules! native_enum {
    ($name:ident { $($variant:ident = $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(match self { $(Self::$variant => $wire,)+ })
            }
        }
        impl NativeWire for $name {
            fn swift_type() -> String { stringify!($name).into() }
            fn contract() -> serde_json::Value { serde_json::json!({"type":"string", "enum":[$($wire),+]}) }
        }
        impl $name {
            pub fn swift() -> String {
                concat!("enum ", stringify!($name), ": String, Codable, Hashable {\n",
                    $("    case ", $wire, " = \"", $wire, "\"\n",)+ "}\n").into()
            }
        }
    };
}

macro_rules! native_struct {
    ($name:ident { $($field:ident: [$ty:ty]),+ $(,)? }) => {
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),+ }
        impl NativeWire for $name {
            fn swift_type() -> String { stringify!($name).into() }
            fn contract() -> serde_json::Value {
                let mut properties = serde_json::Map::new();
                let mut required = Vec::new();
                $(properties.insert(stringify!($field).into(), <$ty as NativeWire>::contract());
                  if !<$ty as NativeWire>::optional() { required.push(stringify!($field)); })+
                // Swift Codable ignores unknown fields, but requires its known
                // fields and closed enum values. This freezes that consumer ABI.
                serde_json::json!({"type":"object", "properties":properties, "required":required})
            }
        }
        impl $name {
            pub fn swift() -> String {
                let mut output = concat!("struct ", stringify!($name), ": Codable {\n").to_owned();
                $(output.push_str(&format!("    let {}: {}\n", stringify!($field),
                    <$ty as NativeWire>::swift_type())) ;)+
                output.push_str("}\n");
                output
            }
        }
    };
}

trait NativeWire {
    fn swift_type() -> String;
    fn contract() -> serde_json::Value;
    fn optional() -> bool {
        false
    }
}
impl NativeWire for String {
    fn swift_type() -> String {
        "String".into()
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"string"})
    }
}
impl NativeWire for bool {
    fn swift_type() -> String {
        "Bool".into()
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"boolean"})
    }
}
impl NativeWire for u32 {
    fn swift_type() -> String {
        "UInt32".into()
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"integer","minimum":0,"maximum":u32::MAX})
    }
}
impl NativeWire for u64 {
    fn swift_type() -> String {
        "UInt64".into()
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"integer","minimum":0,"maximum":u64::MAX})
    }
}
impl NativeWire for u16 {
    fn swift_type() -> String {
        "UInt16".into()
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"integer","minimum":0,"maximum":u16::MAX})
    }
}
impl<T: NativeWire> NativeWire for Option<T> {
    fn swift_type() -> String {
        format!("{}?", T::swift_type())
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"anyOf":[T::contract(),{"type":"null"}]})
    }
    fn optional() -> bool {
        true
    }
}
impl<T: NativeWire> NativeWire for Vec<T> {
    fn swift_type() -> String {
        format!("[{}]", T::swift_type())
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"array","items":T::contract()})
    }
}
impl<T: NativeWire> NativeWire for std::collections::BTreeMap<String, T> {
    fn swift_type() -> String {
        format!("[String: {}]", T::swift_type())
    }
    fn contract() -> serde_json::Value {
        serde_json::json!({"type":"object","additionalProperties":T::contract()})
    }
}

pub const NATIVE_BRIDGE_VERSION: u32 = 6;
pub const NATIVE_BOOTSTRAP_VERSION: u32 = 1;
pub const NATIVE_CLIENT_RELEASE: &str = env!("TYDE_NATIVE_CLIENT_RELEASE");

native_enum!(NativeTab { Home = "home", Agents = "agents", Sessions = "sessions", Settings = "settings" });
native_enum!(NativeLayout { Compact = "compact", RegularPortrait = "regular_portrait", RegularLandscape = "regular_landscape" });
native_enum!(NativeRoute { Root = "root", Chat = "chat", Hosts = "hosts" });
native_enum!(NativeChatHeaderOwner { Shell = "shell", Client = "client" });
native_enum!(NativeChatDockOwner { Shell = "shell", Client = "client" });
native_enum!(NativeRootStatusOwner { Shell = "shell" });
native_enum!(NativeTheme { Dark = "dark", Light = "light" });
native_enum!(NativeIntent {
    Home = "home", Agents = "agents", Sessions = "sessions", Settings = "settings",
    Back = "back", Hosts = "hosts", NewChat = "new_chat", RenameAgent = "rename_agent",
    SendActions = "send_actions", SessionSettings = "session_settings", CloseAgent = "close_agent",
    RegularWidth = "regular_width", CompactWidth = "compact_width",
    RegularLandscape = "regular_landscape", ToggleSidebar = "toggle_sidebar",
    ChatDockShell = "chat_dock_shell", ChatDockClient = "chat_dock_client",
    Foreground = "foreground", Background = "background"
});
native_enum!(NativeEndpoint {
    Presentation = "presentation", Http = "http", HttpCancel = "http_cancel", Authenticate = "authenticate",
    SecretScope = "secret_scope", SecretRead = "secret_read", SecretWrite = "secret_write", SecretDelete = "secret_delete"
});
native_enum!(NativeAuthProvider { Apple = "apple", Google = "google" });
native_enum!(NativeHttpMethod { Get = "get", Post = "post" });

native_struct!(NativeConnectedStatus { round_trip_ms: [Option<u64>] });
native_struct!(NativePresentation {
    version: [u32], client_ready: [bool], workspace: [bool], tab: [NativeTab], route: [NativeRoute],
    title: [String], subtitle: [Option<String>], chat_actions: [bool], modal: [bool], theme: [NativeTheme],
    sidebar_available: [bool], sidebar_open: [bool], chat_header_owner: [Option<NativeChatHeaderOwner>],
    root_status_owner: [Option<NativeRootStatusOwner>], root_connected_status: [Option<NativeConnectedStatus>],
    chat_dock_owner: [Option<NativeChatDockOwner>], chat_connected_status: [Option<NativeConnectedStatus>],
    chat_back_label: [Option<String>]
});
native_struct!(NativeHttpRequest {
    id: [u32],
    url: [String], method: [NativeHttpMethod], body: [Option<String>],
    headers: [std::collections::BTreeMap<String, String>]
});
native_struct!(NativeHttpCancel { id: [u32] });
native_struct!(NativeHttpResponse {
    status: [u16],
    body: [String]
});
native_struct!(NativeSecretKey { key: [String] });
native_struct!(NativeSecretScope { keys: [Vec<String>] });
native_struct!(NativeSecretWrite {
    key: [String],
    value: [String]
});
native_struct!(NativeAuthRequest {
    provider: [NativeAuthProvider]
});

pub fn swift_source() -> String {
    let mut output = format!(
        "// Generated from mobile-shell-types/src/native.rs. Do not edit.\nimport Foundation\n\nlet nativeBridgeVersion: UInt32 = {NATIVE_BRIDGE_VERSION}\n\n"
    );
    output.push_str(&format!(
        "let nativeShellRelease = \"{}\"\nlet nativeBootstrapVersion: UInt32 = {}\n\n",
        NATIVE_CLIENT_RELEASE, NATIVE_BOOTSTRAP_VERSION
    ));
    for definition in [
        NativeTab::swift(),
        NativeRoute::swift(),
        NativeLayout::swift(),
        NativeTheme::swift(),
        NativeChatHeaderOwner::swift(),
        NativeChatDockOwner::swift(),
        NativeRootStatusOwner::swift(),
        NativeConnectedStatus::swift(),
        NativeIntent::swift(),
        NativeEndpoint::swift(),
        NativeHttpMethod::swift(),
        NativeAuthProvider::swift(),
        NativePresentation::swift(),
        NativeHttpRequest::swift(),
        NativeHttpCancel::swift(),
        NativeHttpResponse::swift(),
        NativeSecretKey::swift(),
        NativeSecretScope::swift(),
        NativeSecretWrite::swift(),
        NativeAuthRequest::swift(),
    ] {
        output.push_str(&definition);
        output.push('\n');
    }
    output
}

/// Generated alongside Swift, then frozen once with the first accepted shell.
/// The real DOM guard consumes that old artifact, never today's regenerated ABI.
pub fn wire_contract() -> serde_json::Value {
    serde_json::json!({"bridge_version":NATIVE_BRIDGE_VERSION, "bootstrap_version":NATIVE_BOOTSTRAP_VERSION,
        "intents":NativeIntent::contract(),
        "requests":{
            (NativeEndpoint::Presentation.to_string()):NativePresentation::contract(),
            (NativeEndpoint::Http.to_string()):NativeHttpRequest::contract(),
            (NativeEndpoint::HttpCancel.to_string()):NativeHttpCancel::contract(),
            (NativeEndpoint::Authenticate.to_string()):NativeAuthRequest::contract(),
            (NativeEndpoint::SecretScope.to_string()):NativeSecretScope::contract(),
            (NativeEndpoint::SecretRead.to_string()):NativeSecretKey::contract(),
            (NativeEndpoint::SecretWrite.to_string()):NativeSecretWrite::contract(),
            (NativeEndpoint::SecretDelete.to_string()):NativeSecretKey::contract()
        }
    })
}
