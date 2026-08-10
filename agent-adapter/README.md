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

The built-in adapter declarations are:

| Backend | Sessions | Input/control | Configuration | Usage | Agents/work |
| --- | --- | --- | --- | --- | --- |
| Tycode | list, resume | interrupt | session, MCP, workspace, customization | turn | — |
| ACP | list, resume | interrupt | MCP, workspace, customization | — | — |
| Claude | resume, fork | image, interrupt | session, MCP, workspace, customization | turn | subagents, background, initiated turns |
| Codex | resume, fork | image, interrupt | session, MCP, workspace, customization | turn, request, context | subagents, background |
| Antigravity | resume | interrupt | session, MCP, workspace, customization | — | — |
| Hermes | list, resume | interrupt | session, MCP, workspace, customization | turn, context | subagents, background |

No built-in adapter currently claims authoritative context breakdowns or
mid-turn steering. Background support does not imply autonomous continuation;
only Claude currently declares agent-initiated turns.

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

`CertificationCase` defines 66 narrow live contracts. Every case is an
independent ignored test, so a model can iterate on one invariant with one cheap
provider call. The aggregate `real_universal_backend_qualification_suite` runs
the same catalog across every selected backend and reports all failures it can
reach. Capability-gated cases cover lifecycle and stream identity, follow-up
turns, usage and context, tools, interrupts, resume, workspace instructions,
steering, skills, MCP, images, native subagents, background work, and
agent-initiated continuation.

It never runs as part of ordinary repository validation. Explicitly authorize
real calls and select backends with:

```sh
TYDE_RUN_REAL_AI_TESTS=1 \
TYDE_REAL_BACKENDS=claude,codex,kiro,hermes \
cargo test -p tests --test backend \
  real_universal_backend_qualification_suite -- --ignored --nocapture
```

Run one narrow case while iterating:

```sh
TYDE_RUN_REAL_AI_TESTS=1 TYDE_REAL_BACKENDS=codex \
cargo test -p tests --test backend \
  real_cert_request_sequence_starts_at_one -- --ignored --nocapture
```

`TYDE_REAL_BACKENDS` also accepts `tycode`, `antigravity`, and `agy`. A selected
backend that is missing or unrunnable is a qualification failure, not a silent
skip. Claude qualification calls are pinned to Haiku with `low` effort, and
Codex calls are pinned to `gpt-5.6-luna` with `low` reasoning rather than
inheriting potentially expensive local defaults.
