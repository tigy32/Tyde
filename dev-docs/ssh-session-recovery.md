# SSH session recovery

Managed desktop SSH connections use a resumable transport between the native
router and the remote UDS listener. The SSH bridge remains a byte proxy. Legacy
UDS clients still start with TYD2; recovery clients negotiate a distinct TYR1
preface before sending any Tyde envelopes. A manual custom remote command keeps
its existing transport and receives fresh-session retry rather than resume.

The server issues an opaque, random session ID. This identifies retained state;
it introduces no application authentication. Access still depends on SSH and
Unix socket permissions. Session IDs are scoped to the running host process.

Each endpoint maintains a replay queue of complete serialized envelopes, capped
at 128 MiB and allocated on demand. ACKs release entries immediately. A detached
session expires after five minutes. These limits are per endpoint/session;
serialization, receive buffers, and queue bookkeeping add overhead. No replay
state survives a process restart. Exhaustion invalidates the session instead of
silently dropping an envelope or allowing a gap.

Recovery records have a direction-local monotonic ordinal. ACK N means that
records before N have been delivered into the retained logical transport. The
existing per-stream Tyde sequence validators, stream IDs, host subscriptions,
and command execution loop never restart on a successful resume. Commands whose
responses were lost are not re-executed: the peer's receive position determines
which records need retransmission. This guarantee applies while the logical
session survives; fresh bootstrap never blindly resends uncertain commands.

Only complete envelopes enter the logical byte stream, including after a socket
fails midway through a fragmented transfer. Reader and writer loops remain
independent so outbound backpressure cannot prevent inbound ACKs. New attachments
fence the previous attachment before further logical delivery.

The native shell sends recovery heartbeats every ten seconds and detects an
unresponsive peer after 45 seconds. SSH additionally uses a 15-second keepalive,
three missed replies, and a ten-second connect timeout. Recovery handshakes have
a 15-second deadline. Retry delays grow from immediate to approximately 1, 2, 4,
8, 16, and 30 seconds, with jitter. Retry now wakes the supervisor; disconnect
and removal cancel it. Authentication/configuration/host-key failures stop retries
and surface the diagnostic for user action. Automatic retries do not reinstall
or restart the remote host. A fresh-session fallback uses normal host readiness.

Host status shows disconnected/retrying and catch-up progress. Remote projects,
tabs, files, and drafts remain mounted. Commands are rejected while detached;
voice capture/playback stops. Any already accepted voice envelopes still preserve
wire ordering, but the old voice session is no longer active in the UI/player.
A full bootstrap replaces snapshots and refreshes cached open files and review
subscriptions without destroying tab identity. Terminals survive a successful
resume; on fresh-session fallback their retained views are marked exited because
PTYs belong to the old logical connection. Missing resources can remain open as
unavailable views. Manual host removal performs the destructive cleanup.

Disconnect logs include the initiating transport error, SSH stderr/exit detail,
connection age, and sent/received record counts. The previous investigation did
not find enough real disconnect logs to identify why this user's links dropped;
keepalive settings are resilience measures, not a claimed diagnosis.

Server protocol tests use the real server and shared mock-backend fixture across
breakable transports. They cover command and notification replay, retained stream
sequences, expiration, and buffer exhaustion. Browser tests exercise the rendered
workspace/status through disconnect and fresh-session preparation. Mobile MQTT
and HTTP transports are unchanged by this SSH-specific implementation.

## Native cancellation ownership

A connection generation owns SSH from process launch through initial recovery
negotiation, attachment, and reconnect negotiation. Disconnect and replacement
signal that generation and await its process exit and stderr-task completion.
Pending and retiring generations remain joinable until cleanup completes;
registration and removal compare generation identity so an older attempt cannot
remove a newer retry. Cleanup failures are returned explicitly. Local embedded
connections use the same registry without an SSH child. Native cancellation does
not synthesize frontend connected/disconnected or agent working/idle state.

The real-native regression driver uses the canonical dev-instance launcher, an
owned Xvfb, real loopback OpenSSH, pinned host trust, private one-shot SSH config,
and an authorization-file syscall gate. It covers pending-auth disconnect,
supersession, simultaneous disconnects, an unrelated embedded connection, and
cancellation during reconnect after real recovery and Hello/Welcome handshakes.
It also covers failed-negotiation cleanup and visible public-key rejection.
The owned sshd disables per-source penalties for these deliberate failed-auth
cases; key authorization and strict pinned host trust remain real. It observes the
exact owned SSH processes with pidfds before releasing authentication. The
original native connect Promise must reject specifically from cancellation;
handshake expiry or an authentication rejection cannot pass the regression.
Run in a fresh evidence directory on Linux x86_64, using the controller built by
`./dev.sh check`:

```sh
python3 tools/repro_pending_ssh_cancellation.py \
  "$HOME/tyde-evidence/ssh-cancel-case" target/debug/tyde-server
```

This is opt-in real dev-instance QA, not a provider conformance test or a
replacement for `./dev.sh check`. The fixture requires OpenSSH server/client,
Xvfb, and permission to trace its own descendants; missing prerequisites fail.
It preserves HOME and shared SSH configuration, joins its owned children, and
removes private keys and disposable native stores. Retained JSON/tracing receipts
are local evidence. No UI changes are involved, and this cancellation finding is
not a diagnosis or claimed cure for the separate severe desktop UI stalls.
