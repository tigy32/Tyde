# Tyde Agent Adapter

Shared backend traits, lifecycle contracts, and conformance utilities for
Tyde agent integrations.

This crate depends only on Tyde's wire-level `protocol` crate. Backend
implementations remain in the server and depend on this crate, never the other
way around.

## Capabilities

A capability is a behavioral promise made by an adapter, not a feature that an
upstream provider might support. Capability-gated conformance tests must pass
for every supported version of an adapter before it advertises that capability.

Reported context totals and reported context breakdowns are deliberately
separate. An adapter may know the measured input-token total while only being
able to estimate how those tokens divide between system instructions, tools,
history, reasoning, and injected context.

`BackgroundTasks` permits tool progress to continue after the parent turn goes
idle. `AgentInitiatedTurns` permits a backend to start a new turn without a new
caller input, such as Claude resuming a parent when a background subagent
finishes.

Backend kinds identify agents: Kiro, Claude, Codex, Antigravity, Hermes,
Grok and OpenCode. ACP is shared transport machinery behind concrete backend
implementations. Production capabilities come from each `Backend` implementation;
the [conformance coverage map](../dev-docs/backend-conformance.md) records the
eligible scenarios for each one.

## Conformance validation

`BackendConformanceValidator` consumes accepted inputs, replay boundaries,
chat events, and model-request usage events. It validates:

- user-initiated versus agent-initiated turn admission;
- assistant stream identity, ordering, and terminal uniqueness;
- tool request, progress, and completion correlation;
- cancellation ordering;
- background progress while idle;
- resume replay boundaries;
- provider-request sequence and turn identity;
- monotonic turn and cumulative usage;
- advertised turn, request, context, and breakdown evidence; and
- clean event-stream termination.

The validator is deterministic and makes no provider calls. Live backend tests
can feed it the same normalized events after the adapter-specific parser has
run.

## Paid qualification suite

Each case in `tests/tests/conformance2.rs` runs directly against the production
`Backend` trait and has an independent ignored test per backend. Capability
gates cover lifecycle and stream identity, follow-up turns, usage and context,
tools, interrupts, resume, workspace instructions, steering, skills, MCP,
images, native subagents, background work and agent-initiated continuation.

Run `./dev.sh check` to build and validate the repository. It also builds the
MCP bridge and the conformance executable, but does not run paid scenarios.
Set `CONFORMANCE_BIN` to the resulting executable under
`target/debug/deps/conformance2-<hash>` (not its `.d` dependency file).

After authorizing the real calls under `AGENTS.md`, run a narrow case:

```sh
TYDE_RUN_REAL_AI_TESTS=1 TYDE_REAL_BACKENDS=codex \
TYDE_HERMES_BRIDGE_EXECUTABLE="$PWD/target/debug/tyde-server" \
"$CONFORMANCE_BIN" --ignored --exact real_usage_accounting::codex --nocapture
```

`TYDE_REAL_BACKENDS` accepts `claude`, `codex`, `antigravity`, `kiro`, `hermes`,
`grok` and `opencode`. A selected eligible backend that is missing or unrunnable
is a qualification failure. Capability exclusions are reported separately and
are not backend coverage. Suite profiles choose explicit models and effort;
provider-specific model overrides are available for investigating model drift.
