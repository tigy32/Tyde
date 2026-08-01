# Voice stream contract (protocol 45)

Voice control and signaling use `/voice/<session-id>`. The client creates the
session id. It is an opaque, nonempty UTF-8 path segment of at most 128 bytes;
it must contain neither `/` nor control characters. It is scoped to the owning
host connection and is not required to be a UUID.

The two directions have independent envelope sequence counters. A combined
observer can therefore see `VoiceStart(0)`, `VoiceReady(0)`, `VoiceOffer(1)`,
and `VoiceAnswer(1)` on the same path. Session and target identity never change
after admission.

Only control, SDP, ICE, state, caption, transcript, progress, stop, and error
payloads belong to this stream. Microphone and synthesized audio are WebRTC
audio-track media and never protocol envelopes or MQTT application bytes.
Protocol 45 negotiates Opus for shipping media; PCMU is retained only for
hermetic codec fixtures.

`VoiceError.fatal` is authoritative. A fatal error terminates the voice stream;
a nonfatal error leaves it usable. Server routing errors that tear down their
session are therefore emitted as fatal.
