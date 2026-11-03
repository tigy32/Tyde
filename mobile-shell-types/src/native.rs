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
        impl $name {
            pub fn swift() -> String {
                let mut output = concat!("struct ", stringify!($name), ": Codable {\n").to_owned();
                $(output.push_str(&format!("    let {}: {}\n", stringify!($field),
                    swift_type(stringify!($ty)))) ;)+
                output.push_str("}\n");
                output
            }
        }
    };
}

fn swift_type(ty: &str) -> String {
    let ty = ty.replace(' ', "");
    match ty.as_str() {
        "String" => "String".into(),
        "bool" => "Bool".into(),
        "u32" => "UInt32".into(),
        "u16" => "UInt16".into(),
        "std::collections::BTreeMap<String,String>" => "[String: String]".into(),
        _ => match ty
            .strip_prefix("Option<")
            .and_then(|ty| ty.strip_suffix('>'))
        {
            Some(inner) => format!("{}?", swift_type(inner)),
            None => ty,
        },
    }
}

pub const NATIVE_BRIDGE_VERSION: u32 = 6;
pub const NATIVE_CLIENT_RELEASE: &str = env!("TYDE_NATIVE_CLIENT_RELEASE");

native_enum!(NativeTab { Home = "home", Agents = "agents", Sessions = "sessions", Settings = "settings" });
native_enum!(NativeLayout { Compact = "compact", RegularPortrait = "regular_portrait", RegularLandscape = "regular_landscape" });
native_enum!(NativeRoute { Root = "root", Chat = "chat", Hosts = "hosts" });
native_enum!(NativeTheme { Dark = "dark", Light = "light" });
native_enum!(NativeIntent {
    Home = "home", Agents = "agents", Sessions = "sessions", Settings = "settings",
    Back = "back", Hosts = "hosts", NewChat = "new_chat", RenameAgent = "rename_agent",
    SessionSettings = "session_settings", CloseAgent = "close_agent",
    RegularWidth = "regular_width", CompactWidth = "compact_width",
    RegularLandscape = "regular_landscape", ToggleSidebar = "toggle_sidebar",
    Foreground = "foreground", Background = "background"
});
native_enum!(NativeEndpoint {
    Presentation = "presentation", Http = "http", HttpCancel = "http_cancel", Authenticate = "authenticate",
    SecretRead = "secret_read", SecretWrite = "secret_write", SecretDelete = "secret_delete"
});
native_enum!(NativeAuthProvider { Apple = "apple", Google = "google" });
native_enum!(NativeHttpMethod { Get = "get", Post = "post" });

native_struct!(NativePresentation {
    version: [u32], workspace: [bool], tab: [NativeTab], route: [NativeRoute],
    title: [String], subtitle: [Option<String>], chat_actions: [bool], modal: [bool], theme: [NativeTheme],
    sidebar_available: [bool], sidebar_open: [bool]
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
        "let nativeClientRelease = \"{}\"\nlet nativeProtocolVersion: UInt32 = {}\n\n",
        NATIVE_CLIENT_RELEASE,
        protocol::PROTOCOL_VERSION
    ));
    for definition in [
        NativeTab::swift(),
        NativeRoute::swift(),
        NativeLayout::swift(),
        NativeTheme::swift(),
        NativeIntent::swift(),
        NativeEndpoint::swift(),
        NativeHttpMethod::swift(),
        NativeAuthProvider::swift(),
        NativePresentation::swift(),
        NativeHttpRequest::swift(),
        NativeHttpCancel::swift(),
        NativeHttpResponse::swift(),
        NativeSecretKey::swift(),
        NativeSecretWrite::swift(),
        NativeAuthRequest::swift(),
    ] {
        output.push_str(&definition);
        output.push('\n');
    }
    output
}
