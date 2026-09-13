# Tyde TURN driver patch

Source: crates.io `webrtc` 0.20.5, retaining its upstream license and source.
Changes are confined to `src/peer_connection/driver.rs`,
`src/peer_connection/transports/{tcp_transport,turn_relayer}.rs`, and
`src/runtime/mod.rs`.

The upstream driver drained TURN output before core writes enqueued relay
packets, and processed relay input after draining core output/events. It then
slept with packets still queued until another wake or protocol timeout. Real
local TURN tests showed native handshakes taking about seven seconds, a four
MiB transfer taking 35 seconds, and a browser echo exceeding its 30-second
limit. The native flow intermittently exceeded its unchanged 60-second limit.

Process relay events/reads and their resulting core events before output, then
flush core output before TURN output. This drains both directions in the same
iteration. No timer, timeout, transport policy or test assertion is relaxed.
The actual native/server and browser/native TURN flows in `./dev.sh check`
exercise this patch. Remove it only after an upstream version passes those
same flows without it.

TLS TURN support adds a runtime-provided, certificate-verified TLS connector and
permits relay-only peers without local UDP sockets or TCP listeners. TURN stream
records use their STUN/ChannelData headers and four-byte ChannelData padding per
RFC 8656; ICE-TCP retains its existing framing. The client-to-relay connection is
TCP, while the requested TURN allocation and relayed ICE candidate remain UDP.
The application supplies the Rustls TLS runtime and validates certificates and
hostnames; runtimes without TLS support return an explicit unsupported error.

The real TLS/TURN tests exercise certificate and hostname rejection, ordered bulk
transfer with backpressure, server reconnection, and browser/native interop.

A live Chrome client sending directly from `onopen` exposed another ordering
bug: relay input produced both `OnOpen` and a data message, but `poll_reads`
delivered the message before the next `poll_events` delivered `OnOpen`. The
native adapter correctly rejected data before opening. Drain those newly
produced core events before delivering their messages. The browser flow now
sends immediately from `onopen`; it reproduced the rejection before this fix
and still checks the complete bulk transfer and reconnects afterward.
