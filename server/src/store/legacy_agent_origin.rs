//! Drops the persisted agent origin session forks used to carry, before a
//! store parses into the typed [`protocol::AgentOrigin`].
//!
//! Forks are now ordinary top-level agents with `AgentOrigin::User`, so the
//! `side_question` variant no longer exists. `AgentOrigin` rejects unknown
//! variants, so a saved filter or smart view still naming it would make the
//! whole preferences file read as corrupt and cost the user every saved view.
//!
//! The entry is dropped rather than renamed to `user`: a view that asked for
//! side questions would otherwise silently start matching every ordinary
//! chat. Losing the criterion is the honest outcome — the category is gone.
//!
//! Like the backend-kind rewrite next door, this is key-directed so a tag,
//! team, or agent a user happened to name `side_question` survives untouched.

use protocol::LEGACY_SIDE_QUESTION_ORIGIN;
use serde_json::Value;

/// Key whose value is an array of agent origins.
const LIST_KEY: &str = "origins";

/// Removes every persisted `"side_question"` origin anywhere in the document.
/// Returns whether anything changed, so callers can decide whether the file
/// needs rewriting.
pub(crate) fn drop_legacy_side_question_origins(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for (key, entry) in map.iter_mut() {
                if key == LIST_KEY
                    && let Some(items) = entry.as_array_mut()
                {
                    let before = items.len();
                    items.retain(|item| item.as_str() != Some(LEGACY_SIDE_QUESTION_ORIGIN));
                    changed |= items.len() != before;
                }
                changed |= drop_legacy_side_question_origins(entry);
            }
            changed
        }
        Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
            changed | drop_legacy_side_question_origins(item)
        }),
        _ => false,
    }
}
