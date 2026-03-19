# Agent Control

Tyde can give your coding agents the ability to spawn and manage other agents. This is how multi-agent workflows work in Tyde — one agent acts as a coordinator, delegates tasks to sub-agents, waits for their results, and combines their work. All of this happens through a local MCP server that Tyde runs in the background.

## Bridge chats

The primary way to use agent control is through bridge chats. When you click **New Bridge Chat** on the home screen, Tyde starts a coordination agent that has the agent control server pre-configured. A bridge agent doesn't do implementation work itself — it delegates to sub-agents. You describe what you want, and the bridge agent breaks the work into tasks, spawns agents to handle each one, monitors their progress, and reports back.

The bridge chat has access to all your open projects, so it can spawn agents across different workspaces. You can also choose which backend the bridge agent itself uses from the dropdown next to the button.

## Enabling agent control

The agent control MCP server is enabled by default. You can toggle it in **Settings → Agent Control → Enable Loopback MCP Control**.

## What agents can do

When an agent has access to Tyde's agent control server, it gains the ability to:

**Delegate work to sub-agents.** An agent can spawn a new agent with a specific task, wait for it to finish, and use the result. This is the most common pattern — ask an agent to do something complex and it breaks the work into pieces, handing each piece to a sub-agent.

**Run multiple agents in parallel.** An agent can spawn several sub-agents at once and wait for any of them to finish. For example, a coordinator might spawn three agents to fix three different files simultaneously, then review all the results when they're done.

**Mix backends in a single workflow.** Sub-agents don't have to use the same backend as their parent. A Tycode coordinator could spawn a Claude Code agent for one task and a Codex agent for another, then compare or combine their output.

**Send follow-up messages.** An agent can send additional instructions to a running sub-agent without restarting it.

**Cancel agents.** An agent can interrupt and shut down a sub-agent that's no longer needed.

## What you see in Tyde

Sub-agents appear in the Agents panel, nested under their parent. You can monitor their progress, click into their conversations to see what they're doing, or interrupt them from the panel — the same as any other agent. Everything the sub-agent does (file changes, diffs, terminal commands) shows up in Tyde's UI as normal.

## When to use this

Agent control is most useful when you have a large task that can be broken into independent pieces. Instead of one agent working through everything sequentially, a coordinator can delegate pieces in parallel and finish faster. It's also useful when different parts of a task benefit from different backends — for example, using one backend for frontend work and another for backend work.

Start a bridge chat from the home screen and describe your task. The bridge agent will handle the coordination automatically.
