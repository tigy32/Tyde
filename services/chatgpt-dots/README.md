# ChatGPT Dots connection service

This Cloudflare Worker is the hosted OAuth and transport boundary for
Settings → Integrations → ChatGPT Dots. Each host establishes an outbound
authenticated WebSocket; no OpenAI Platform credentials or inbound host ports
are required. The public MCP endpoint forwards requests only to the host
selected by the authenticated OAuth grant. Tyde owns tool execution and live
agent state. The broker does not reconstruct agents or persist tool payloads.

## First connection

1. Enable ChatGPT Dots on the selected Tyde host. Tyde creates owner-only host
   credentials, registers the host, opens its connection, and displays a
   one-time, ten-minute pairing code.
2. In ChatGPT Plugins, add a custom MCP server using the displayed URL and
   OAuth authentication. Create and install the plugin.
3. In the Tyde authorization page opened by ChatGPT, enter the pairing code
   and authorize access. Do not paste the code into a model conversation.

ChatGPT account/workspace permissions still apply. Public-directory submission
is separate and is not performed by this implementation. There is no supported
API for Tyde to silently install a plugin into someone's ChatGPT account.

Disabling the integration closes the host connection and independently blocks
tool execution on the host. Already-running agents continue. Re-enabling uses
the existing host registration and OAuth grant. Disconnect ChatGPT revokes
the host registration, invalidating all associated access/refresh tokens, and
removes its local credentials before offering a new pairing.

## Security and scope

- Host keys and OAuth tokens are 256-bit random values. The broker stores
  hashes, not raw host keys, pairing codes, access tokens, or refresh tokens.
- OAuth uses single-use authorization codes, mandatory S256 PKCE, exact
  registered ChatGPT callbacks, resource binding, one-hour access tokens, and
  rotating thirty-day refresh tokens. Issuer identification is advertised and
  returned in authorization responses.
- The user's pairing code is the proof of host ownership. Authorization grants
  `tyde:control`: access to this host's projects and agent control. It does not
  grant access to another host. Tool annotations distinguish reads and writes.
- Tyde uses an explicit external-controller path, not an invented Tyde agent
  identity or another agent's bearer credentials.
- TLS protects each transport hop. The connection service and OpenAI process
  tool inputs/results; this is not end-to-end encryption. No transcript, key,
  pairing code, or raw payload is emitted in service diagnostics.
- Requests and replies are size-bounded; request concurrency, OAuth expiry,
  host isolation, and offline/error behavior are enforced at the boundary.

The first version exposes project/launch-option/agent lists and agent spawn,
read, message, and interruption. It does not expose arbitrary shell execution,
configuration changes, direct merges, releases, or proactive MCP Events. Agents
can use Tyde's existing configured orchestration/review tools as before.

## Operation and validation

The relay and host-registration wire codecs are generated from the canonical
Rust protocol. Do not hand-edit `codec.mjs`. After changing those types, build
the native host through `./dev.sh check`, then regenerate source with
`target/debug/tyde-server dots-schema | python3 tools/generate-dots-codec.py --write`.
The real Worker flow checks that the compiled host's schemas reproduce the
tracked codec before exercising HTTP routing. Generation is not validation;
rerun `./dev.sh check` after committing generated changes.

`./dev.sh check` runs the real Worker in workerd through Miniflare, the server
protocol scenario against that Worker and the permitted mock backend, and the
real-DOM settings scenario in Chrome. Do not run their underlying commands
directly. `serve.mjs` is the local real-service fixture used by the server flow.

The deployed Worker requires one SQLite Durable Object binding named `BROKER`
to the exported `DotsBroker` class. Deployment must retain the existing binding
and migration history. Production uses the public HTTPS origin in
`server/src/dots.rs`; deployment and OAuth account consent are not release tags.
