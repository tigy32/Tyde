# Tyde TURN driver patch

Source: crates.io `webrtc` 0.20.5, retaining its upstream license and source.
Only `src/peer_connection/driver.rs` differs from that release.

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
