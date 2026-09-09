use super::*;
use protocol::{
    BackendConfigSnapshotStatus, BackendKind, BackendNativeSettingsGroup,
    BackendNativeSettingsGroupKind, BackendNativeSettingsSnapshot,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SettingsDocument {
    version: String,
    values: serde_json::Map<String, Value>,
}

/// Every reasoning effort Codex's configuration accepts, independent of model.
/// The model catalog reports the subset a given model supports; this is the
/// superset used where no catalog entry applies.
const REASONING_EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

struct Field {
    key: &'static str,
    title: &'static str,
    group: &'static str,
    description: &'static str,
    schema: Value,
}

fn fields(models: &[Value], config: &Value) -> Vec<Field> {
    let model = config.get("model").and_then(Value::as_str);
    let selected = models
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == model)
        .or_else(|| {
            models
                .iter()
                .find(|entry| model.is_none() && entry.get("isDefault") == Some(&Value::Bool(true)))
        });
    let model_ids = models
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let mut speed = vec!["default"];
    if let Some(tiers) = selected
        .and_then(|entry| entry.get("serviceTiers"))
        .and_then(Value::as_array)
    {
        speed.extend(
            tiers
                .iter()
                .filter_map(|tier| tier.get("id").and_then(Value::as_str))
                .filter(|id| *id != "default"),
        );
    }
    // The catalog reports the efforts a known model supports. A model absent
    // from the catalog — a custom provider model, or one the catalog retired —
    // resolves to no entry, which would otherwise leave this field with an
    // empty, unselectable list; fall back to every effort Codex accepts.
    let effort = selected
        .and_then(|entry| entry.get("supportedReasoningEfforts"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option.get("reasoningEffort").and_then(Value::as_str))
                .collect::<Vec<_>>()
        })
        .filter(|options| !options.is_empty())
        .unwrap_or_else(|| REASONING_EFFORTS.to_vec());
    let choices = |options: Vec<&str>| {
        let mut options = options
            .into_iter()
            .map(|value| json!(value))
            .collect::<Vec<_>>();
        options.push(Value::Null);
        json!({"type":["string","null"],"enum":options})
    };
    let boolean = || json!({"type":["boolean","null"]});
    let integer = || json!({"type":["integer","null"],"minimum":1});
    vec![
        Field {
            key: "model",
            title: "Default model",
            group: "defaults",
            description: "Model for new chats. Session choices take precedence.",
            schema: choices(model_ids.clone()),
        },
        Field {
            key: "model_reasoning_effort",
            title: "Default reasoning effort",
            group: "defaults",
            description: "Reasoning effort for new chats using the default model.",
            schema: choices(effort),
        },
        Field {
            key: "service_tier",
            title: "Default speed",
            group: "defaults",
            description: "Service tier for new chats. Default means standard speed; accelerated tiers can cost more and require account access.",
            schema: choices(speed),
        },
        Field {
            key: "agents.enabled",
            title: "Enable Codex subagents",
            group: "subagents",
            description: "Controls Codex's native subagents. Tyde's subagent settings are separate.",
            schema: boolean(),
        },
        Field {
            key: "agents.max_concurrent_threads_per_session",
            title: "Maximum concurrent subagents",
            group: "subagents",
            description: "Maximum open Codex subagents per session, excluding the main agent.",
            schema: integer(),
        },
        Field {
            key: "agents.default_subagent_model",
            title: "Default subagent model",
            group: "subagents",
            description: "Model for native subagents unless the spawning agent chooses one explicitly.",
            schema: choices(model_ids),
        },
        Field {
            key: "agents.default_subagent_reasoning_effort",
            title: "Default subagent reasoning effort",
            group: "subagents",
            description: "Default effort for native subagents; the selected subagent model must support it.",
            schema: choices(REASONING_EFFORTS.to_vec()),
        },
        Field {
            key: "personality",
            title: "Communication style",
            group: "responses",
            description: "Applies to models that support personality selection.",
            schema: choices(vec!["none", "friendly", "pragmatic"]),
        },
        Field {
            key: "model_verbosity",
            title: "Response verbosity",
            group: "responses",
            description: "Preferred response length for models supporting verbosity.",
            schema: choices(vec!["low", "medium", "high"]),
        },
        Field {
            key: "model_reasoning_summary",
            title: "Reasoning summaries",
            group: "responses",
            description: "Preferred detail for supported reasoning summaries. Tyde requests automatic summaries for its own chats, so this applies to Codex used outside Tyde.",
            schema: choices(vec!["auto", "concise", "detailed", "none"]),
        },
        Field {
            key: "features.memories",
            title: "Enable Codex memory",
            group: "memory",
            description: "Enable Codex's native memory feature, where supported by the installed CLI and account.",
            schema: boolean(),
        },
        Field {
            key: "memories.generate_memories",
            title: "Generate new memories",
            group: "memory",
            description: "Allow new threads to contribute to memory generation when memory is enabled.",
            schema: boolean(),
        },
        Field {
            key: "memories.use_memories",
            title: "Use saved memories",
            group: "memory",
            description: "Include saved memories in future sessions when memory is enabled.",
            schema: boolean(),
        },
        Field {
            key: "memories.disable_on_external_context",
            title: "Exclude external context from memory",
            group: "memory",
            description: "Exclude threads using MCP, web search, or tool search from memory generation.",
            schema: boolean(),
        },
        Field {
            key: "web_search",
            title: "Web search",
            group: "advanced",
            description: "Cached searches use the search index; live searches fetch current results. Availability depends on the provider.",
            schema: choices(vec!["disabled", "cached", "live"]),
        },
        Field {
            key: "model_auto_compact_token_limit",
            title: "Automatic compaction threshold",
            group: "advanced",
            description: "Context tokens that trigger automatic compaction. Unset uses the model default.",
            schema: integer(),
        },
        Field {
            key: "tool_output_token_limit",
            title: "Tool output token limit",
            group: "advanced",
            description: "Maximum tool-output tokens retained in context.",
            schema: integer(),
        },
        Field {
            key: "allow_login_shell",
            title: "Allow login shells",
            group: "advanced",
            description: "Allow shell tools to load login-shell configuration.",
            schema: boolean(),
        },
    ]
}

fn config_value<'a>(config: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(config, |value, part| value.get(part))
}

/// Codex's raw model catalog, keyed by model slug.
///
/// `model/list` reports a curated view that omits the per-model default
/// verbosity and reasoning summary, so those come from `codex debug models`
/// instead. Best effort by design: a missing binary, a failed run, or an
/// unrecognized shape yields no entries, and the affected fields then report no
/// default rather than a guessed one.
async fn raw_model_catalog() -> serde_json::Map<String, Value> {
    let Ok(mut command) = codex_command() else {
        return serde_json::Map::new();
    };
    command.arg("debug").arg("models");
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    let Ok(Ok(output)) = tokio::time::timeout(CODEX_CAPACITY_PROBE_TIMEOUT, command.output()).await
    else {
        return serde_json::Map::new();
    };
    if !output.status.success() {
        return serde_json::Map::new();
    }
    let Ok(entries) = serde_json::from_slice::<Vec<Value>>(&output.stdout) else {
        return serde_json::Map::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let slug = entry.get("slug").and_then(Value::as_str)?.to_owned();
            Some((slug, entry))
        })
        .collect()
}

/// The value Codex applies to `key` when the user's configuration does not set
/// it, read from Codex itself — never assumed.
///
/// Two sources report a real default. Codex resolves some fields in its
/// effective configuration, where a value present without a user override is
/// the default in force. The rest of Codex's defaults are per-model and appear
/// only in the model catalog. Fields Codex reports nowhere get no default here;
/// the page states that instead of inventing one.
fn field_default(
    key: &str,
    config: &Value,
    raw: &Value,
    models: &[Value],
    selected: Option<&Value>,
    catalog: &serde_json::Map<String, Value>,
) -> Option<Value> {
    // A user's own override is not a default, including one still written
    // under the legacy subagent key this page reads it from.
    let overridden = config_value(raw, key).or_else(|| {
        (key == "agents.max_concurrent_threads_per_session")
            .then(|| config_value(raw, "agents.max_threads"))
            .flatten()
    });
    if overridden.is_some_and(|value| !value.is_null()) {
        return None;
    }
    let catalog_entry = || {
        selected
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_str)
            .and_then(|slug| catalog.get(slug))
    };
    let catalog_value = |field: &str| {
        catalog_entry()
            .and_then(|entry| entry.get(field))
            .filter(|value| !value.is_null())
            .cloned()
    };
    match key {
        "model" => models
            .iter()
            .find(|entry| entry.get("isDefault") == Some(&Value::Bool(true)))
            .and_then(|entry| entry.get("id"))
            .cloned(),
        "model_reasoning_effort" => selected
            .and_then(|entry| entry.get("defaultReasoningEffort"))
            .filter(|value| !value.is_null())
            .cloned(),
        "model_verbosity" => catalog_value("default_verbosity"),
        "model_reasoning_summary" => catalog_value("default_reasoning_summary"),
        // Whatever Codex resolved without a user override — its own default, or
        // one an administrator set in the system configuration layer.
        _ => config_value(config, key)
            .filter(|value| !value.is_null())
            .cloned(),
    }
}

fn user_layer(response: &Value) -> Result<&Value, String> {
    response
        .get("layers")
        .and_then(Value::as_array)
        .and_then(|layers| {
            layers.iter().find(|layer| {
                layer.pointer("/name/type").and_then(Value::as_str) == Some("user")
                    && layer.pointer("/name/profile").is_none_or(Value::is_null)
            })
        })
        .ok_or_else(|| "Codex did not report a writable user configuration layer".to_owned())
}

async fn open_config() -> Result<CodexRpc, String> {
    let (rpc, _) = CodexRpc::spawn_with_local_program(
        None,
        &[],
        None,
        BackendAccessMode::Unrestricted,
        BackendExecutionMode::Agent,
        None,
    )
    .await?;
    let initialized = tokio::time::timeout(CODEX_CAPACITY_PROBE_TIMEOUT, rpc.request("initialize", json!({
        "clientInfo":{"name":"tyde-settings","version":"1"}, "capabilities":{"experimentalApi":true}
    }))).await.map_err(|_| "Codex settings initialization timed out".to_owned()).and_then(|result| result);
    if let Err(error) = initialized {
        let _ = rpc.terminate().await;
        return Err(error);
    }
    Ok(rpc)
}

async fn read_config(rpc: &CodexRpc) -> Result<(Value, Vec<Value>), String> {
    let response = rpc
        .request("config/read", json!({"includeLayers":true}))
        .await?;
    let models = rpc
        .request("model/list", json!({"includeHidden":false}))
        .await?;
    let models = models
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Codex model catalog is unavailable")?
        .clone();
    Ok((response, models))
}

fn snapshot(
    response: &Value,
    models: &[Value],
    catalog: &serde_json::Map<String, Value>,
) -> Result<BackendNativeSettingsSnapshot, String> {
    let layer = user_layer(response)?;
    let version = layer
        .get("version")
        .and_then(Value::as_str)
        .ok_or("Codex user config version is missing")?
        .to_owned();
    let config = response
        .get("config")
        .ok_or("Codex effective config is missing")?;
    let raw = layer.get("config").ok_or("Codex user config is missing")?;
    let fields = fields(models, config);
    // The model whose per-model defaults apply: the configured one, or the
    // catalog's default when no model is configured.
    let configured = config.get("model").and_then(Value::as_str);
    let selected = models
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == configured)
        .or_else(|| {
            models.iter().find(|entry| {
                configured.is_none() && entry.get("isDefault") == Some(&Value::Bool(true))
            })
        });
    let mut values = serde_json::Map::new();
    for field in &fields {
        let value = config_value(raw, field.key).or_else(|| {
            (field.key == "agents.max_concurrent_threads_per_session")
                .then(|| config_value(raw, "agents.max_threads"))
                .flatten()
        });
        if let Some(value) = value.filter(|value| !value.is_null()) {
            values.insert(field.key.to_owned(), value.clone());
        }
    }
    let mut groups = Vec::new();
    for (id, title) in [
        ("defaults", "Defaults"),
        ("subagents", "Subagents"),
        ("responses", "Responses"),
        ("memory", "Memory"),
        ("advanced", "Advanced"),
    ] {
        let mut properties = serde_json::Map::new();
        for (order, field) in fields.iter().filter(|field| field.group == id).enumerate() {
            let mut schema = field.schema.clone();
            schema["title"] = json!(field.title);
            schema["x-tyde-order"] = json!(order);
            if field.key == "service_tier" {
                let mut labels =
                    serde_json::Map::from_iter([("default".to_owned(), json!("Standard"))]);
                for model in models {
                    if let Some(tiers) = model.get("serviceTiers").and_then(Value::as_array) {
                        for tier in tiers {
                            if let (Some(id), Some(name)) = (
                                tier.get("id").and_then(Value::as_str),
                                tier.get("name").and_then(Value::as_str),
                            ) {
                                labels.insert(id.to_owned(), json!(name));
                            }
                        }
                    }
                }
                schema["x-tyde-enum-labels"] = json!(labels);
            }
            schema["x-tyde-reset-label"] = json!("Use CLI default");
            if let Some(options) = schema.get_mut("enum").and_then(Value::as_array_mut)
                && let Some(value) = values.get(field.key)
                && !options.contains(value)
            {
                options.push(value.clone());
            }
            let default = field_default(field.key, config, raw, models, selected, catalog);
            if let Some(default) = &default {
                schema["x-tyde-default"] = default.clone();
                schema["x-tyde-default-label"] = json!("Codex default");
            }
            let inherited = config_value(config, field.key).filter(|value| !value.is_null());
            schema["description"] = json!(match (&default, inherited) {
                // The control renders the default itself, so the description
                // only has to say what leaving the field unset means.
                (Some(_), _) => format!("{} Unset uses Codex's default.", field.description),
                (None, Some(value)) => format!(
                    "{} Current effective CLI value: {value}. Unset removes your user override.",
                    field.description
                ),
                (None, None) =>
                    format!("{} Unset lets Codex choose its default.", field.description),
            });
            properties.insert(field.key.to_owned(), schema);
        }
        groups.push(BackendNativeSettingsGroup {
            id:id.to_owned(), title:title.to_owned(), kind:BackendNativeSettingsGroupKind::Core,
            settings_path:vec!["values".to_owned()],
            description:Some("Saved to Codex's base user configuration on this host, including use outside Tyde. CLI profiles, project settings, and session choices can override these defaults. Running sessions are not reconfigured; startup behavior applies on the next process launch.".to_owned()),
            schema:json!({"type":"object","properties":properties,"additionalProperties":false}),
        });
    }
    Ok(BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Codex,
        status: BackendConfigSnapshotStatus::Ready,
        settings: Some(
            serde_json::to_value(SettingsDocument { version, values })
                .map_err(|error| error.to_string())?,
        ),
        groups,
        message: None,
        advisories: vec![],
    })
}

pub(crate) async fn native_settings_snapshot() -> BackendNativeSettingsSnapshot {
    let result = async {
        let rpc = open_config().await?;
        // Only the snapshot needs the raw catalog; saving reads the same
        // configuration without it, so a save never pays for this.
        let catalog = raw_model_catalog().await;
        let result = tokio::time::timeout(CODEX_CAPACITY_PROBE_TIMEOUT, async {
            let (response, models) = read_config(&rpc).await?;
            snapshot(&response, &models, &catalog)
        })
        .await
        .map_err(|_| "Codex settings read timed out".to_owned())
        .and_then(|result| result);
        codex_probe_result_with_cleanup(result, rpc.terminate().await)
    }
    .await;
    result.unwrap_or_else(|message| BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Codex,
        status: BackendConfigSnapshotStatus::Unavailable,
        settings: None,
        groups: vec![],
        message: Some(message),
        advisories: vec![],
    })
}

pub(crate) async fn persist_native_settings(settings: Value) -> Result<(), String> {
    let document: SettingsDocument =
        serde_json::from_value(settings).map_err(|error| error.to_string())?;
    let rpc = open_config().await?;
    let result = tokio::time::timeout(CODEX_CAPACITY_PROBE_TIMEOUT, async {
        let (response, models) = read_config(&rpc).await?;
        let layer = user_layer(&response)?;
        if layer.get("version").and_then(Value::as_str) != Some(document.version.as_str()) {
            return Err("Codex settings changed since this page loaded. Review the refreshed values and try again.".to_owned());
        }
        let raw = layer.get("config").ok_or("Codex user configuration is missing")?;
        let mut effective = response.get("config").cloned().ok_or("Codex effective configuration is missing")?;
        if let Some(model) = document.values.get("model").filter(|value| !value.is_null()) { effective["model"] = model.clone(); }
        let fields = fields(&models, &effective);
        for key in document.values.keys() {
            if !fields.iter().any(|field| field.key == key) { return Err(format!("Unknown Codex setting: {key}")); }
        }
        let mut edits = Vec::new();
        for field in fields {
            let next = document.values.get(field.key).unwrap_or(&Value::Null);
            let previous = config_value(raw, field.key).or_else(|| (field.key == "agents.max_concurrent_threads_per_session").then(|| config_value(raw, "agents.max_threads")).flatten()).unwrap_or(&Value::Null);
            if next == previous { continue; }
            if !next.is_null() {
                let valid = match field.schema["type"][0].as_str() {
                    Some("boolean") => next.is_boolean(),
                    Some("integer") => next.as_i64().is_some_and(|value| value >= 1),
                    Some("string") => next.as_str().is_some() && field.schema["enum"].as_array().is_some_and(|options| options.contains(next)),
                    _ => false,
                };
                if !valid { return Err(format!("Invalid value for {}. Check the selected model's supported options.", field.title)); }
            }
            edits.push(json!({"keyPath":field.key,"value":next,"mergeStrategy":"replace"}));
            if field.key == "agents.max_concurrent_threads_per_session" && config_value(raw,"agents.max_threads").is_some() {
                edits.push(json!({"keyPath":"agents.max_threads","value":null,"mergeStrategy":"replace"}));
            }
        }
        if !edits.is_empty() {
            rpc.request("config/batchWrite",json!({"edits":edits,"expectedVersion":document.version,"filePath":layer.pointer("/name/file"),"reloadUserConfig":false})).await?;
        }
        Ok(())
    }).await.map_err(|_| "Codex settings save timed out; refresh to check whether it completed".to_owned()).and_then(|result| result);
    codex_probe_result_with_cleanup(result, rpc.terminate().await)
}
