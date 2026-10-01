# Tyde SCTP packet-size patch

Source: crates.io `rtc-sctp` 0.20.5, with its upstream MIT/Apache-2.0
licensing and source retained. The only source change is `INITIAL_MTU` in
`src/config.rs`, from 1228 to 1100 bytes.

The upstream budget excludes DTLS, TURN, UDP and IP overhead. In Tyde's real
TLS/TURN fixture it produces 1269-byte UDP datagrams. On a minimum-MTU IPv6
path the UDP budget is 1232 bytes. Small handshakes succeed, but larger data
and their unchanged retransmissions are discarded, permanently blocking
the ordered stream. See https://github.com/webrtc-rs/webrtc/issues/806.

The reduced SCTP budget reserves room for DTLS record protection (including
the default CBC suite's IV, MAC and padding), TURN framing (including IPv6
Data indications), and UDP/IPv6 headers. This changes SCTP fragmentation,
not Tyde's logical message sizes, acknowledgement window, or deadlines.

The existing real relay fixture now models the 1232-byte UDP limit. Without
this patch, six mobile protocol/bootstrap flows, the voice/bulk flow and
the transport bulk/reconnect flow time out after opening their channels.
Browser/native interop uses the same constrained relay in the canonical
check. Remove the patch only when an upstream version passes those flows.
