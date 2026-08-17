//! Rewrites the persisted spelling of a renamed backend kind before a store
//! parses into the typed [`protocol::BackendKind`].
//!
//! `BackendKind` rejects unknown variants, so a store still holding `"kiro"`
//! does not degrade gracefully — the whole file fails to deserialize and the
//! store reports itself corrupt. Stores that hand-filter unknown kinds
//! (settings) can rename in place; the ones here parse straight into the typed
//! enum, so the rename has to happen on the raw JSON first.
//!
//! The rewrite is deliberately key-directed rather than a blanket string
//! replace: only values under a key that actually holds a backend kind are
//! touched, so a team, workflow, or agent that a user happened to name "kiro"
//! survives untouched.

use protocol::{ACP_BACKEND, LEGACY_KIRO_BACKEND};
use serde_json::Value;

/// Keys whose string value is a single backend kind.
const SCALAR_KEYS: [&str; 3] = ["backend_kind", "backend", "default_backend"];

/// Keys whose value is an array of backend kinds.
const LIST_KEYS: [&str; 2] = ["backends", "enabled_backends"];

/// Renames every persisted `"kiro"` backend kind to `"acp"` anywhere in the
/// document. Returns whether anything changed, so callers can decide whether
/// the file needs rewriting.
pub(crate) fn rewrite_legacy_kiro_backend_kinds(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for (key, entry) in map.iter_mut() {
                if SCALAR_KEYS.contains(&key.as_str()) {
                    changed |= rename_scalar(entry);
                } else if LIST_KEYS.contains(&key.as_str())
                    && let Some(items) = entry.as_array_mut()
                {
                    for item in items.iter_mut() {
                        changed |= rename_scalar(item);
                    }
                }
                changed |= rewrite_legacy_kiro_backend_kinds(entry);
            }
            changed
        }
        Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
            changed | rewrite_legacy_kiro_backend_kinds(item)
        }),
        _ => false,
    }
}

fn rename_scalar(value: &mut Value) -> bool {
    if value.as_str() == Some(LEGACY_KIRO_BACKEND) {
        *value = Value::String(ACP_BACKEND.to_string());
        return true;
    }
    false
}
