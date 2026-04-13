use std::fs;
use std::path::Path;

use strum::VariantNames;
use ts_rs::{Config, TS};
use tyde_protocol::protocol::*;

fn main() -> Result<(), String> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let generated_root = repo_root.join("packages/protocol/src/generated");
    let rust_root = generated_root.join("rust");
    let protocol_src_root = repo_root.join("packages/protocol/src");
    let legacy_bindings = repo_root.join("bindings");

    recreate_dir(&rust_root)?;
    match fs::remove_dir_all(&legacy_bindings) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to remove stale bindings directory {}: {error}",
                legacy_bindings.display()
            ));
        }
    }

    let cfg = Config::default()
        .with_out_dir(&rust_root)
        .with_large_int("number");

    export_protocol_types(&cfg)?;

    write_rust_index(&rust_root)?;
    let generated_type_names = collect_export_names(&rust_root)?;
    write_protocol_constants(&generated_root)?;
    write_protocol_kinds(&generated_root)?;
    write_command_map(&protocol_src_root, &generated_type_names)?;
    write_action_map(&protocol_src_root, &generated_type_names)?;
    write_event_map(&protocol_src_root, &generated_type_names)?;

    Ok(())
}

fn export_protocol_types(cfg: &Config) -> Result<(), String> {
    macro_rules! export_types {
        ($($ty:ty),+ $(,)?) => {{
            $(
                <$ty>::export_all(cfg).map_err(|e| format!("{e:?}"))?;
            )+
        }};
    }

    export_types!(
        ChatEvent,
        ChatActorMessage,
        ContextInfo,
        Model,
        SessionData,
        ConversationRegisteredData,
        ChatEventPayload,
        ConversationRegisteredPayload,
        AdminEventPayload,
        ClientFrame,
        ServerFrame,
        ConversationSnapshot,
        HandshakeResult<serde_json::Value, serde_json::Value, serde_json::Value>,
        ImageAttachment,
        GitFileStatus,
        FileEntry,
        FileContent,
        BackendKind,
        RuntimeAgent,
        ToolPolicy,
        AgentMcpTransportHttp,
        AgentMcpTransportStdio,
        AgentMcpTransport,
        AgentMcpServer,
        AgentDefinition,
        DefinitionScope,
        AgentDefinitionEntry,
        DialogKind,
        RuntimeAgentEvent,
        RuntimeAgentEventBatch,
        SpawnAgentResponse,
        AgentResult,
        AwaitAgentsResponse,
        CollectedAgentResult,
        McpHttpServerSettings,
        DriverMcpHttpServerSettings,
        BackendDepResult,
        BackendDependencyStatus,
        DevInstanceStartParams,
        DevInstanceStartResult,
        DevInstanceStopParams,
        DevInstanceStopResult,
        DevInstanceInfo,
        WorkflowScope,
        WorkflowActionEntry,
        WorkflowStepEntry,
        WorkflowEntry,
        ShellCommandResult,
        SessionRecord,
        CreateAgentResponse,
        RemoteKind,
        Host,
        RemoteControlSettings,
        RemoteServerStatus,
        BackendUsageWindow,
        BackendUsageResult,
        RemoteTydeServerState,
        RemoteTydeServerStatus,
        ProjectRecord,
        FileChangedPayload,
        TerminalOutputPayload,
        TerminalExitPayload,
        RemoteConnectionProgress,
        ReconnectingAttempt,
        DisconnectedReason,
        TydeServerConnectionScalarState,
        TydeServerConnectionStateValue,
        TydeServerConnectionState,
        TydeServerVersionWarning,
        ProjectsChangedPayload,
        HostsChangedPayload,
        HostProjectsChangedPayload,
        CreateWorkbenchPayload,
        DeleteWorkbenchPayload,
        AppBootstrap,
        TydeEventEnvelope,
        DispatchRequest,
        DesktopRequest,
        DebugRequest,
        DebugUiRequestPayload,
        AwaitAgentsParams,
        RegisterGitWorkbenchParams,
    );

    Ok(())
}

fn recreate_dir(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to remove generated directory {}: {error}",
                path.display()
            ));
        }
    }
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "Failed to create generated directory {}: {error}",
            path.display()
        )
    })
}

fn write_rust_index(rust_root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    collect_ts_exports(rust_root, rust_root, &mut files)?;
    files.sort();

    let mut out = generated_header();
    for (name, relative_path) in files {
        out.push_str(&format!(
            "export type {{ {name} }} from \"./{relative_path}\";\n"
        ));
    }

    fs::write(rust_root.join("index.ts"), out)
        .map_err(|error| format!("Failed to write generated Rust TS index: {error}"))
}

fn collect_export_names(rust_root: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    collect_ts_exports(rust_root, rust_root, &mut files)?;
    files.sort();
    Ok(files.into_iter().map(|(name, _)| name).collect())
}

fn collect_ts_exports(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|error| format!("Failed to read {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Failed to read {}: {error}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some("index.ts") {
            continue;
        }
        if path.is_dir() {
            collect_ts_exports(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("ts") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("Failed to relativize {}: {error}", path.display()))?
            .to_string_lossy()
            .trim_end_matches(".ts")
            .replace('\\', "/");
        out.push((stem.to_string(), relative));
    }
    Ok(())
}

fn write_protocol_constants(generated_root: &Path) -> Result<(), String> {
    let out = format!(
        "{}\nexport const GENERATED_PROTOCOL_VERSION = {} as const;\n",
        generated_header(),
        PROTOCOL_VERSION
    );

    fs::write(generated_root.join("protocol_constants.ts"), out)
        .map_err(|error| format!("Failed to write protocol constants: {error}"))
}

fn write_protocol_kinds(generated_root: &Path) -> Result<(), String> {
    let out = format!(
        "{}\n{}\n\n{}\n\n{}\n",
        generated_header(),
        ts_const(
            "GENERATED_CHAT_EVENT_KINDS",
            ChatEvent::VARIANTS,
            "GeneratedChatEventKind"
        ),
        ts_const(
            "GENERATED_CHAT_ACTOR_MESSAGE_VARIANTS",
            ChatActorMessage::VARIANTS,
            "GeneratedChatActorMessageVariant"
        ),
        ts_const(
            "GENERATED_MODEL_VARIANTS",
            Model::VARIANTS,
            "GeneratedModelVariant"
        )
    );

    fs::write(generated_root.join("protocol_kinds.ts"), out)
        .map_err(|error| format!("Failed to write protocol kinds: {error}"))
}

fn write_command_map(
    protocol_src_root: &Path,
    generated_type_names: &[String],
) -> Result<(), String> {
    let cfg = Config::default().with_large_int("number");
    let mut out = generated_header();
    write_generated_type_import(&mut out, generated_type_names);
    out.push_str("export interface CommandMap {\n");
    for spec in COMMAND_SPECS {
        out.push_str(&format!(
            "  {}: {{\n    params: {};\n    response: {};\n  }};\n",
            spec.name,
            (spec.params_ts)(&cfg),
            (spec.response_ts)(&cfg)
        ));
    }
    out.push_str("}\n\n");
    out.push_str("export type CommandName = keyof CommandMap;\n");
    out.push_str("export type CommandParams<K extends CommandName> = CommandMap[K][\"params\"];\n");
    out.push_str(
        "export type CommandResponse<K extends CommandName> = CommandMap[K][\"response\"];\n\n",
    );
    out.push_str("export type DesktopOnlyCommand =\n");
    write_union(
        &mut out,
        COMMAND_SPECS
            .iter()
            .filter(|spec| spec.desktop_only)
            .map(|spec| spec.name),
    );
    out.push_str(";\n\n");
    out.push_str("export type SharedCommand = Exclude<CommandName, DesktopOnlyCommand>;\n");

    fs::write(protocol_src_root.join("commands.ts"), out)
        .map_err(|error| format!("Failed to write commands.ts: {error}"))
}

fn write_action_map(
    protocol_src_root: &Path,
    generated_type_names: &[String],
) -> Result<(), String> {
    let cfg = Config::default().with_large_int("number");
    let mut out = generated_header();
    write_generated_type_import(&mut out, generated_type_names);
    out.push_str("export interface DesktopActionMap {\n");
    for spec in DESKTOP_ACTION_SPECS {
        out.push_str(&format!(
            "  {}: {{\n    params: {};\n    response: {};\n  }};\n",
            spec.name,
            (spec.params_ts)(&cfg),
            (spec.response_ts)(&cfg)
        ));
    }
    out.push_str("}\n\n");
    out.push_str("export type DesktopActionName = keyof DesktopActionMap;\n");
    out.push_str(
        "export type DesktopActionParams<K extends DesktopActionName> = DesktopActionMap[K][\"params\"];\n",
    );
    out.push_str(
        "export type DesktopActionResponse<K extends DesktopActionName> = DesktopActionMap[K][\"response\"];\n\n",
    );
    out.push_str("export interface DebugActionMap {\n");
    for spec in DEBUG_ACTION_SPECS {
        out.push_str(&format!(
            "  {}: {{\n    params: {};\n    response: {};\n  }};\n",
            spec.name,
            (spec.params_ts)(&cfg),
            (spec.response_ts)(&cfg)
        ));
    }
    out.push_str("}\n\n");
    out.push_str("export type DebugActionName = keyof DebugActionMap;\n");
    out.push_str(
        "export type DebugActionParams<K extends DebugActionName> = DebugActionMap[K][\"params\"];\n",
    );
    out.push_str(
        "export type DebugActionResponse<K extends DebugActionName> = DebugActionMap[K][\"response\"];\n",
    );

    fs::write(protocol_src_root.join("actions.ts"), out)
        .map_err(|error| format!("Failed to write actions.ts: {error}"))
}

fn write_event_map(
    protocol_src_root: &Path,
    generated_type_names: &[String],
) -> Result<(), String> {
    let cfg = Config::default().with_large_int("number");
    let mut out = generated_header();
    write_generated_type_import(&mut out, generated_type_names);
    out.push_str("export interface EventMap {\n");
    for spec in EVENT_SPECS {
        out.push_str(&format!(
            "  {}: {};\n",
            quoted(spec.name),
            (spec.payload_ts)(&cfg)
        ));
    }
    out.push_str("}\n\n");
    out.push_str("export type EventName = keyof EventMap;\n");
    out.push_str("export type EventPayload<K extends EventName> = EventMap[K];\n\n");
    out.push_str("export type DesktopOnlyEvent =\n");
    write_union(
        &mut out,
        EVENT_SPECS
            .iter()
            .filter(|spec| spec.desktop_only)
            .map(|spec| spec.name),
    );
    out.push_str(";\n\n");
    out.push_str("export type SharedEvent = Exclude<EventName, DesktopOnlyEvent>;\n");

    fs::write(protocol_src_root.join("events.ts"), out)
        .map_err(|error| format!("Failed to write events.ts: {error}"))
}

fn write_generated_type_import(out: &mut String, generated_type_names: &[String]) {
    out.push_str("import type {\n");
    for name in generated_type_names {
        out.push_str(&format!("  {name},\n"));
    }
    out.push_str("} from \"./generated/rust/index\";\n\n");
}

fn write_union<'a>(out: &mut String, values: impl Iterator<Item = &'a str>) {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        out.push_str(" never");
        return;
    }
    for (idx, value) in values.iter().enumerate() {
        if idx == 0 {
            out.push_str(&format!("  {}", quoted(value)));
        } else {
            out.push_str(&format!("\n  | {}", quoted(value)));
        }
    }
}

fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}

fn generated_header() -> String {
    "// This file is auto-generated by cargo run -p tyde-protocol --bin generate_typescript\n// Do not edit by hand.\n\n".to_string()
}

fn ts_const(name: &str, variants: &[&str], type_name: &str) -> String {
    let values = variants
        .iter()
        .map(|variant| format!("  \"{variant}\","))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "export const {name} = [\n{values}\n] as const;\n\nexport type {type_name} = (typeof {name})[number];"
    )
}
