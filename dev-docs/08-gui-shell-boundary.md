# Frontend / Shell Boundary

This document freezes the frontend boundary for Tyde2.

It follows `01-philosophy.md` directly:

- one source of truth
- server owns behavior
- transport layers stay dumb
- frontend renders state

## Crate Roles

### `frontend`

The `frontend` crate is the Rust frontend.

It owns:

- protocol types from the `protocol` crate
- construction of protocol messages
- parsing of protocol messages
- frontend state derived from protocol events
- all UI behavior and rendering

If the frontend wants to send `hello`, `spawn_agent`, project user-intent
events, or any future protocol frame, the `frontend` crate builds that message
itself.

### `tauri-shell`

The `tauri-shell` crate is a Tauri transport shell nested under `frontend/`.

It owns:

- opening connections to hosts
- closing connections to hosts
- forwarding ordinary control envelopes between the GUI and a host
- native microphone/output device lifetime for accepted voice sessions
- native echo cancellation, 20 ms Opus encoding, and Opus decoding/playback

It does **not** own ordinary application semantics. Its only protocol-aware
exception is the shared record codec and the narrow `VoiceAudio` validation
needed to keep raw PCM out of IPC. It does not own:

- non-voice `FrameKind` branching
- non-voice payload interpretation
- payload structs
- sequence counters
- agent/project/session state
- protocol branching
- backend semantics

The shell is otherwise a byte proxy with host connection ownership. Voice is
the deliberate media exception: raw PCM never crosses WebView IPC.

## API Shape

The shell API is protocol-agnostic except for the native voice media boundary:

- `connect_host`
- `disconnect_host`
- `send_host_line`
- `send_host_frame` for a typed envelope plus a bounded Opus body
- `voice_media_start`, `voice_media_push_output`, `voice_media_stop`
- emit `tyde://host-line`
- emit `tyde://host-disconnected`
- emit `tyde://host-error`

The payload crossing the shell boundary is just:

- host identity
- transport config
- ordinary envelope JSON text, or a bounded Opus packet with its typed envelope

Tauri managed state holds only the native-audio thread's bounded control
handle. CPAL streams, device state, AEC, Opus codecs, and active media identity
never enter managed state or WebView IPC. Lifecycle commands are acknowledged
after resource drop and use bounded fail-closed waits.

The shell may inspect only the voice media envelope needed to bind Opus to a
session and generation. It must not own agent behavior or expose PCM arrays.

## Consequence

If a future feature needs new protocol data:

1. add it in `protocol`
2. implement it in `server`
3. handle it in `frontend`

Do **not** add interpretation logic to `tauri-shell`.
