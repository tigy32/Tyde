# Host Settings

Server-owned host settings delivered over the `/host/*` stream. Builds on
`01-philosophy.md`, `02-protocol.md`, and `04-host-registry.md`.

Managed mobile broker access has an additional product/service boundary:
`30-mobile-managed-broker.md` owns the Tyggs Pass, `tycode.dev`, and AWS IoT
contract. Host settings may expose server-owned mobile controls, but they must
not store Tyggs account data, pass proofs, billing state, or production broker
fallbacks that bypass `tycode.dev`.

---

## Problem

The rewrite had a settings overlay, but backend configuration was still local
frontend state:

- The frontend chose a default backend with a local signal.
- The backend settings page rendered static cards.
- New-chat behavior depended on frontend-local state rather than host state.
- There was no protocol event for current host settings and no persistence in
  `tyde-server`.

That violated the design rules:

- The server must own behavior; the UI only renders state.
- State flows through events, not hidden caches.
- Everything must use protocol types end-to-end.

The fix is a typed host settings model in `settings-model`, persisted and owned
by `tyde-server`, replayed on connect, and updated through compare-and-swap
host-stream operations.

---

## Scope

The persisted host-settings document covers:

- `enabled_backends`
- `default_backend`
- native voice enablement, AWS profile/region selectors, and exact Nova model

These are host-level settings, not per-session settings or frontend
preferences. Backend-native settings are also exposed on the host stream, but
remain a separate backend-owned domain and are never written into the host
settings store.

Out of scope for this slice:

- MCP settings
- agent definitions
- notifications
- advanced settings
- host registry / multi-host selection

---

## Protocol Model

Host settings are strongly typed in `settings-model/src/lib.rs`; protocol owns
only the generic transport payloads.

### Types

```rust
pub struct HostSettings {
    pub enabled_backends: Vec<BackendKind>,
    pub default_backend: Option<BackendKind>,
    pub voice: VoiceSettings,
}
```

`BackendKind` is already an enum, so no stringly-typed backend identifiers are
introduced here.

### Input Events

Clients update settings on `/host/<uuid>` with `FrameKind::SettingsWrite`.
Each `SettingOp::Replace` or `SettingOp::Remove` carries an RFC 6901 path and a
mandatory value, version-token, or absent compare-and-swap expectation. A
write is atomic: overlapping operations or any failed expectation reject the
whole batch.

Profile create/delete and similar backend lifecycle operations use
`FrameKind::InvokeSettingsAction`, not document writes.

### Output Event

The server emits:

```rust
FrameKind::HostSettings
```

Payload:

```rust
pub struct HostSettingsPayload<HostSettings> {
    pub settings: HostSettings,
    pub etag: String,
    pub configured_secrets: Vec<ConfiguredSecret>,
}
```

`HostBootstrap` supplies initial state; `HostSettings` is the authoritative
live-update shape.

---

## Semantics

### Bootstrap and snapshots

`HostBootstrap` carries the initial redacted settings document, its etag, the
build-static JSON Schema, and revision tokens for configured write-only paths.
An applied write fans out `HostSettings` with the new full redacted snapshot,
etag, and configured-secret tokens.

### `settings_write`

Under one settings-apply lock, the host checks path knowledge, secret rules,
overlap, and every CAS expectation; applies the batch to a candidate document;
deserializes and validates the typed model; commits the host store; propagates
backend effects; and publishes the snapshot. The requester alone receives a
correlated `SettingsWriteResult`. Semantic rejection is a typed result, not a
`CommandError`.

The client does not optimistically replace server-owned settings. It waits for
the authoritative fanout.

## Invariants

The server enforces:

- `enabled_backends` may be empty
- if `default_backend` is set, it must be present in `enabled_backends`

Normalization rules:

- backend lists are canonicalized in fixed enum order
- duplicate/unknown values are not preserved
- if the store file is missing, the server returns an empty settings snapshot
- invalid persisted settings fail load instead of being silently repaired

There is no protocol-level backend default in this slice.

If `enabled_backends` is changed such that the current `default_backend` is no
longer enabled, the host clears `default_backend` to `null` and emits that
updated snapshot.

---

## Persistence

Host settings are persisted in a dedicated server-owned store:

- default path: `~/.tyde/settings.json`
- override: `TYDE_SETTINGS_STORE_PATH`

This lives beside the existing session and project stores, but remains a
separate file because it is a separate domain model.

Current store shape:

```json
{
  "settings": {
    "enabled_backends": [],
    "default_backend": null
  }
}
```

The store is read on demand and replaced atomically on write, matching the
existing store pattern used by sessions and projects.

---

## Server Ownership

The host actor owns the settings lifecycle.

### Registration Replay

When a host stream is registered, the host replays:

1. `host_settings`
2. existing projects
3. existing agents

That order is intentional:

- settings are general host state
- projects are host-owned inventory
- agents are runtime instances

This keeps startup state replay aligned with the "events in, events out" model.

### Mutation Fanout

When settings change, the host fans out the latest `host_settings` snapshot to
all connected host subscribers.

No special-case frontend refresh logic is required beyond normal event handling.

---

## Frontend Data Flow

The frontend stores host-keyed settings, schemas, and configured-secret tokens.
They are seeded by `HostBootstrap`; settings and secret tokens are replaced by
each `HostSettings` fanout.

### Settings Overlay

The settings overlay renders complex editors from the typed model and primitive
rows from the server-published schema. Edits emit path-scoped `SettingsWrite`
operations against the selected host.

### Runtime Behavior

`spawn_new_chat` reads `host_settings.default_backend` when creating a new
agent. If no default is configured, the frontend does not invent one.

That is the intended direction: settings should affect runtime via server-owned
state, not isolated frontend preferences.

---

## Tycode Native Settings

Tycode settings are backend-owned rather than part of persisted
`HostSettings`. The server publishes typed `BackendNativeSettingsSnapshot`
events; the frontend renders them and never reads TOML, inspects files, parses
error text to infer recovery, or maintains a parallel settings model. Saves use
`BackendNativeSettingsWrite`; profile lifecycle commands use
`InvokeSettingsAction`. They are never persisted in the ordinary host settings
store.

Upstream added `GetSettingsSchema` in 0.9.3-pre.1. Tyde adopts grouped native
settings at its exact stable 0.10.0 pin and renders the returned top-level
`core` and nested `module` groups.

### Installed runtime identity

Tyde runs only the checksum-pinned installed artifact:

```text
~/.tyde/tycode/0.10.0/tycode-subprocess
```

There is no login-shell `PATH` lookup, out-of-tree executable, semver range,
older-version fallback, or downgrade. Compatibility requires a regular file,
a successful `--version` exit, empty stderr, and exact
`tycode-subprocess 0.10.0` stdout apart from its conventional final line
ending. Exact text with a non-zero exit is rejected. A missing installed
artifact is `NotInstalled` on supported platforms; a present artifact that
fails identity is `Unavailable` with the server diagnostic.

Every actor, native/legacy probe and save, new session, resume, default
initialization, normalization, and verification process uses the mandatory
command builder with exactly one explicit `--settings-path`. Normal work uses
`~/.tycode/tyde-settings.toml`; transaction work uses its owned private stage.

### Managed and shared ownership

The files have deliberately separate owners:

- shared CLI/VS Code source: `~/.tycode/settings.toml`;
- Tyde-managed settings: `~/.tycode/tyde-settings.toml`;
- managed provenance: `~/.tycode/tyde-settings.provenance.json`;
- transaction journal: `~/.tycode/tyde-settings.transaction.json`;
- cross-process lock: `~/.tycode/tyde-settings.lock`.

Tyde may read and byte-copy the shared source while deriving a managed copy,
and rechecks those bytes before publication. Tyde never passes the shared path
to Tycode and never writes, renames, truncates, chmods, normalizes, deletes, or
repairs it. This is a guarantee about Tyde's behavior, not a claim that another
program has not changed the shared file since derivation.

The sibling managed path keeps Tycode sessions under
`~/.tycode/sessions`; `tyde-settings.toml` also avoids named-profile filename
semantics. After publication the copies intentionally diverge. There is no
automatic re-import, watch, merge, export, or shared-file fallback.

### Exact default and TOML semantics

Lazy derivation begins by asking pinned Tycode to initialize a unique settings
path that genuinely does not exist. Tyde does not pre-create an empty file.
After Tycode creates its canonical default TOML, a fresh second process must
return matching typed settings and groups.

If the shared source is absent, that verified default becomes the staged
managed copy. If a shared source exists, Tyde byte-copies its real TOML to a
separate private stage, probes it, and compares its typed settings semantics
with the verified default:

- empty, comments-only, explicit-default, or prune-to-default input uses the
  Tycode-created default artifact and never sends persistent `SaveSettings`;
- only non-default input uses persistent normalization on the private stage,
  followed by fresh-process verification.

This avoids Tycode v0.10's intentional `Refusing to persist empty settings`
failure without matching an error string or fabricating a success. The
deterministic Tycode test process is required to read and write real TOML,
create defaults for nonexistent paths, reproduce that semantic-default refusal,
prune the same modeled unknowns, expose Tycode's injected `profile` in typed
schemas, and omit `profile` from persisted TOML.

### Cross-process lock

The mode-`0600` OS filesystem lock is authoritative across Tyde processes; the
process-local async lock is only an additional in-process coordinator. The OS
lock covers inspection, startup recovery, creation, save, notice
acknowledgement, reset, and managed-artifact cleanup.

A live lock is never stolen based on PID or elapsed time. After an owner exits,
the next holder safely replaces stale or malformed owner metadata, including a
PID-reuse record. No process may inspect and then delete another live process's
transaction state outside this lock.

### Durable journal and recovery

Creation, save, notice acknowledgement, and reset are durable pair
transactions. Private same-directory stages and backups have recorded SHA-256
identities. Every file is synced before the journal may refer to it; every
publication, journal phase transition, rename, and cleanup is followed by a
directory sync.

On startup under the OS lock, the server uses journal phase plus recorded
old/new hashes to prove and complete or roll back the pair. Rename order alone
is never treated as proof. A crash during publication or cleanup resumes
idempotently. Pre-journal private transaction stages are removed only while the
lock is held.

If recovery can prove neither pair, the server preserves the evidence, writes a
typed recovery checkpoint, and publishes `ManagedProjectionResetRequired`.
There is no best-effort destructive repair, provenance-orphan deletion rule,
shared-file recovery, or fallback. Unsafe types/ownership/permissions, invalid
version/path/provenance, and unexplained hash mismatches remain visible server
errors or recovery state rather than guessed-safe input.

A normal settings save likewise occurs only on a private stage, is verified in
a fresh process, and is journaled with its prior and target pair. The frontend
does not treat its input value as proof of success; the host force-emits the
authoritative refreshed snapshot.

### Phase-aware advisories

An error-authored `MessageAdded` received only while waiting for the initial
`SessionStarted` is retained as typed `NoProviderConfigured` or
`BackendReported` advisory and probing continues. Structured errors are always
fatal. Error-authored messages after the settings command, invalid event order,
timeout, early exit, failed save, missing result, or malformed schema remain
fatal with their actual phase and earlier advisory context.

A successful schema remains `Ready` and editable with advisories. A missing
usable provider yields `NoProviderConfigured`. An `active_provider` missing
from the normalized provider map yields `UnsupportedActiveProvider`; its visual
surface uses polite `role="status"`, remains editable, and says only that Tycode
v0.10 cannot model it in Tyde's copy, Tyde did not remove it, and Tyde never
writes the shared CLI/VS Code file. It does not claim what that shared file
currently contains.

### Disclosure and notice acknowledgement

Every managed snapshot carries typed provenance. A persistent ownership line
states that Tyde uses its managed file and never modifies the CLI/VS Code file.
The one-time notice likewise describes Tyde's no-write behavior and never
claims the shared file is currently unchanged or still contains a value.

Notice dismissal sends
`AcknowledgeTycodeProjectionNotice { backend, projection_id }`. Runtime checks
the exact ID while holding the OS lock and journals the provenance update. A
stale ID is typed `CommandErrorCode::Conflict`; the server does not infer or
retry with a newer ID, and the UI does not optimistically hide the notice.

### Typed managed-copy reset

Reset availability comes only from the server-emitted field:

```rust
managed_projection_recovery:
    Option<TycodeManagedProjectionRecoveryState>
```

Its only current state is:

```rust
ManagedProjectionResetRequired {
    reason,
    expected_projection_id,
    expected_state_hash,
}
```

The UI never derives reset availability from an `Unavailable` message or local
filesystem state. `ManagedProjectionResetRequired` means the current process
already attempted automatic journal recovery and could not prove a valid pair.
The recovery card explains that restarting Tyde only retries that same repair;
it may clear the state if the cause was a transient filesystem failure, but it
is not guaranteed. If the typed recovery state remains after restart, reset is
the confirmed remedy.

Confirmation sends the exact tokens from the snapshot the user saw:

```rust
ResetTycodeManagedProjection {
    backend: BackendKind::Tycode,
    expected_projection_id,
    expected_state_hash,
}
```

The warning states that Tyde's managed copy, provenance, recovery checkpoint,
journal, and reserved transaction artifacts are cleared; Tyde-only edits will
be lost; the shared CLI/VS Code file is never touched; and a fresh managed copy
is built on the next ordinary probe. This lazy re-derivation after explicit
reset is the sole exception to the normal no-re-import lifecycle. It is
replacement after informed destructive confirmation, not a merge, repair, or
fallback.

Runtime rechecks both typed tokens under the OS lock. A stale projection ID,
state hash, or already-recovered state returns typed
`CommandErrorCode::Conflict` and removes nothing. An accepted reset is itself
journaled and crash-resumable, clears only the managed transaction inventory,
never removes the lock or shared file, and leaves the next ordinary request to
derive lazily. The host force-refreshes native/config snapshots after success,
Conflict, or runtime failure so the client receives current recovery state.
The UI never hides the card optimistically; only a new server snapshot without
the recovery field removes it.

### Inherited v0.10 behavior

Tycode 0.10.0 canonicalizes workspace roots. Code comparing a Tycode tool path
with a Tyde workspace root must canonicalize Tyde's root first; on macOS,
`/tmp/x` may be reported as `/private/tmp/x`.

Tycode also injects a gitignore-respecting recursive project-tree snapshot for
each workspace root into conversational root-agent turns (`tycode` and
`one_shot`). It may truncate the snapshot according to `auto_context_bytes`.
Sub-agents retain leaner context. Tyde reports backend token usage and context
breakdown as emitted and does not compensate for this backend policy.

### Corrective validation status

The corrective default, TOML, lock, journal, identity, disclosure, and reset
implementation is awaiting the canonical `./dev.sh check`. Historical checks
and QA do not validate this final corrective behavior.

---

## Why This Design

This design follows the philosophy document directly:

- One source of truth: `HostSettings` lives in `settings-model`.
- Server owns behavior: persistence, validation, and fanout all happen in
  `tyde-server`.
- UI only renders state: the frontend does not own backend settings anymore.
- Bootstrap supplies initial state; `host_settings` supplies authoritative
  updates.
- The protocol transports generic JSON operations while server and clients share
  the typed settings model.

---

## Next Steps

Natural extensions add typed fields to `HostSettings`, expose primitive rows
through its schema annotations, and keep complex editors in Rust. No new wire
enum is needed for ordinary fields. Settings remain server-owned, and schema
validation on the client is advisory; the server is authoritative.
