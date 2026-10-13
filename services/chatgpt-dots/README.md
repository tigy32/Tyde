# Remote MCP connection service

This Cloudflare Worker is the hosted OAuth and transport boundary for
Settings → Integrations → Remote MCP. Each host establishes an outbound
authenticated WebSocket; no OpenAI Platform credentials or inbound host ports
are required. The public MCP endpoint forwards requests only to the host
selected by the authenticated OAuth grant. Tyde owns tool execution and live
agent state. The broker does not reconstruct agents or persist tool payloads.

## Enable Remote MCP

1. Enable Remote MCP on the selected Tyde host. Tyde creates owner-only host
   credentials, registers the host, opens its connection, and displays a
   one-time, ten-minute pairing code.
2. Configure an OAuth-capable remote MCP client with
   `https://mcp.tycode.dev/mcp`. The service publishes OAuth discovery and
   supports dynamic registration of public clients using the authorization-code
   flow. Register the client's exact redirect URI before authorization.
3. In the Tyde authorization page opened by that client, enter the pairing code
   and consent to control of the selected host. Do not paste the code into a
   model conversation.

Client registration accepts HTTPS callbacks or explicitly registered native
loopback HTTP callbacks. Non-loopback HTTP and custom-scheme callbacks are not
supported. S256 PKCE, resource binding, exact registered redirects (including
loopback ports), and host-code consent are mandatory for every client; generic
client support does not relax these checks. Clients must support this OAuth
contract, not merely accept an MCP URL. ChatGPT is the first supported setup
guide; this does not claim tested compatibility with every other client.

One connection selects one host, and every grant is bound to that host. There
is no cross-host aggregation or second connection system for ChatGPT. The
persisted `chatgpt_dots_enabled` setting, protocol `Dots` names, Worker name,
and storage names remain unchanged for compatibility; Remote MCP is the
user-facing name for the same integration, not an additional boolean.

Disabling Remote MCP closes the host connection and independently blocks tool
execution on the host. Already-running agents continue. Re-enabling uses the
existing host registration and OAuth grant. Disconnect revokes the host
registration, invalidating all associated access/refresh tokens, and removes
its local credentials before offering a new pairing.

## ChatGPT setup guide

Use the same enabled Remote MCP host and its pairing code; there is no separate
ChatGPT enable switch.

1. In Tyde, **Open ChatGPT** copies the server URL and opens personal Plugins.
   In ChatGPT, select the plus button, then **Add custom MCP server**.
2. Name the connection **Tyde**, enter `https://mcp.tycode.dev/mcp` as the server
   URL, and choose **OAuth** with dynamic client registration, without a
   manually supplied client ID or secret. Review the risk warning, then create
   and install the plugin.
3. When ChatGPT opens the Tyde authorization page, enter the selected host's
   pairing code and approve access. Keep the code out of chat messages.
4. Use the installed plugin in a chat and ask it to list this host's Tyde
   projects before starting an agent. Spawning is not completion: read the
   agent's result separately.

ChatGPT account/workspace permissions still apply. See the official OpenAI
[custom MCP server guide](https://developers.openai.com/api/docs/guides/custom-mcp-server)
for the current ChatGPT connection UI. Public-directory submission is separate
and is not performed by this implementation; Tyde does not silently install a
plugin into someone's ChatGPT account. The official guide does not document
a form-prefill URL; Tyde copies the URL for pasting rather than inventing
query parameters or automating account consent.

## Security and scope

- Host keys and OAuth tokens are 256-bit random values. The broker stores
  hashes, not raw host keys, pairing codes, access tokens, or refresh tokens.
- OAuth uses single-use authorization codes, mandatory S256 PKCE, exact
  registered client callbacks, resource binding, one-hour access tokens, and
  rotating thirty-day refresh tokens. Issuer identification is advertised and
  returned in authorization responses.
- The user's pairing code is the proof of host ownership. Authorization grants
  `tyde:control`: access to this host's projects and agent control. It does not
  grant access to another host. Tool annotations distinguish reads and writes.
- Tyde uses an explicit external-controller path, not an invented Tyde agent
  identity or another agent's bearer credentials.
- TLS protects hosted transport hops; native HTTP callbacks are confined to
  loopback. The connection service and selected MCP client process tool
  inputs/results; this is not end-to-end encryption. No transcript, key,
  pairing code, or raw payload is emitted in service diagnostics.
- Service responses set `no-store, no-transform` to keep consent private and
  prevent Cloudflare from injecting analytics scripts into the authorization
  page; zone-wide analytics settings are not changed.
- Requests and replies are size-bounded; request concurrency, OAuth expiry,
  host isolation, and offline/error behavior are enforced at the boundary.

The same seven tools are exposed to every authorized client:

- `tyde_list_projects`
- `tyde_list_launch_options`
- `tyde_list_agents`
- `tyde_spawn_agent`
- `tyde_read_agent`
- `tyde_send_agent_message`
- `tyde_interrupt_agent`

The integration does not expose arbitrary shell execution, configuration
changes, direct merges, releases, or proactive MCP Events. Agents can use
Tyde's existing configured orchestration/review tools as before.

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

## Production Custom Domain

The canonical origin is `https://mcp.tycode.dev`; new Remote MCP connections use
`https://mcp.tycode.dev/mcp`. Attach this hostname to the **existing production
`tyde-dots` Worker in the same account**, not a new Worker or preview environment.
The deploy script only accepts this exact custom hostname with `--name tyde-dots`.
The already-attached `dots.tycode.dev` Custom Domain and legacy
`tyde-dots.crystal-depths.workers.dev` route must remain enabled and unchanged.
The script snapshots their current configuration and never rewrites the dots
binding, renames the account's `crystal-depths.workers.dev` subdomain, disables
legacy routing, deletes DNS records, or replaces another domain binding.
Other Workers, routes, and Custom Domains remain untouched.

Cloudflare's official [Custom Domains guide](https://developers.cloudflare.com/workers/configuration/routing/custom-domains/)
requires an active zone and an existing Worker. Cloudflare manages the DNS record
and certificate when the domain is attached; do not create a CNAME to workers.dev.
The script uses the official [List Worker Domains](https://developers.cloudflare.com/api/resources/workers/subresources/domains/methods/list/)
and [Attach Worker Domain](https://developers.cloudflare.com/api/resources/workers/subresources/domains/methods/update/)
APIs (`GET`/`PUT /accounts/{account_id}/workers/domains`), targeting service
`tyde-dots` with the accessible `tycode.dev` zone, hostname routing enabled, and
preview routing disabled. It does not use a bulk route replacement or any
DNS/domain overwrite option.

Run this **read-only preflight** from the repository root:

```bash
(
  set +x
  set -a
  . "$HOME/.secrets"
  set +a
  python3 services/chatgpt-dots/deploy.py \
    --name tyde-dots --custom-domain mcp.tycode.dev --preflight
)
```

Credentials are `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN`, sourced from
`~/.secrets` without printing their values. The token needs account Workers
Scripts access (Write for deployment) and zone/zone-DNS/Worker-route read access
for preflight. An inaccessible or inactive zone is a provisioning blocker, not
permission to onboard a zone, rename an account, or bypass checks. Preflight
does not prove write permissions or certificate readiness.

Preflight requires the existing production `BROKER` namespace, enabled
`dots.tycode.dev` Custom Domain, and enabled legacy workers.dev route. It checks
account ownership and active status of `tycode.dev`, exact-host DNS records,
Custom Domain ownership/environment/routing, and overlapping Worker routes,
including wildcard routes. Any unrelated DNS record at `mcp.tycode.dev`,
conflicting binding/route, incomplete API listing, or unsupported Worker setting
stops the operation without modifying Cloudflare.
An already-correct production Custom Domain is retained without rewriting it or
its DNS records. Missing API access fails closed; no fallback lookup is used.

**Only after integration review and validation**, the integrator may deploy and
attach the domain with this exact command:

```bash
(
  set +x
  set -a
  . "$HOME/.secrets"
  set +a
  python3 services/chatgpt-dots/deploy.py \
    --name tyde-dots --custom-domain mcp.tycode.dev --confirm
)
```

The confirmed operation repeats preflight before upload and immediately before
attachment. Upload retains the `BROKER` binding, namespace, migration history,
and deployment settings; other existing bindings use strict inheritance rather
than resubmitting masked secrets. Production is never recreated and receives no
new Durable Object migration. Only a new explicitly selected `tyde-dots-preview`
Worker may provision fresh preview storage. The script checks the deployed
`BROKER`, dots Custom Domain, and legacy route before attaching the new domain;
it checks the dots binding again after attachment without rewriting it. Upload
and attachment are separate API operations: if attachment fails, the Worker may
already be updated, but no conflicting DNS or domain is deliberately replaced.
Serialize production deployments and DNS/domain administration while this
command runs; the Custom Domain API does not provide an atomic check-and-attach
operation.

Before shipping hosts with the new origin, verify HTTPS certificate readiness
and the OAuth discovery endpoints on all three hostnames, with each reporting
its own issuer and `/mcp` resource and neither old issuer redirecting to the new
one. A successful attach API response alone does not establish public HTTPS
readiness.

### Existing-grant compatibility

Keep both `https://dots.tycode.dev` and
`https://tyde-dots.crystal-depths.workers.dev` reachable **without redirects**.
All three hostnames reach the same existing `BROKER` namespace and the same
named `dots` Durable Object. Do not migrate or recreate storage. Host
credentials and registrations remain unchanged, so upgraded hosts reconnect at
`mcp.tycode.dev` without recreating their identity.

Existing connections continue using their original OAuth issuer endpoints and
`/mcp` resource on either old hostname. Their tokens are not interchangeable
with the new resource; new connections use `mcp.tycode.dev`. Do not rewrite an
existing connection's issuer or audience, redirect an old issuer, or silently
migrate its tokens. Adding the hostname does not change grant expiry/rotation,
revocation, host isolation, or relay behavior. Generic OAuth client registration
broadens eligible callbacks but retains mandatory PKCE, exact registered
redirects, resource binding, and explicit host-code consent. Deployment can
reconnect live host sockets, but retained storage preserves grants; a revoked
or expired grant is not restored.
