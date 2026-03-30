# Agent Definitions

An agent definition is a reusable template that configures how a coding agent behaves. You define a name, description, custom instructions, and optional settings like a default backend or agent control access. When you spawn an agent from a definition, Tyde applies the template to the new conversation automatically. This is useful when you want consistent behavior from agents — for example, a code reviewer that always focuses on security, or a documentation writer that follows a specific style.

## Creating a definition

Open **Settings → Agents** and click **+ New Agent**. The modal has the following fields:

**Name** — A human-readable name for the agent (e.g. "Code Reviewer"). The ID is generated automatically from the name.

**Description** — A brief summary of what this agent does. This is shown in the agents list and passed to backends that support agent identity metadata.

**Instructions** — Custom system instructions delivered to the agent at the start of every conversation. Write these as you would a system prompt — they tell the agent what role to play, what to focus on, and what to avoid. For example: "You are a code reviewer. Focus on correctness, security, and readability. Do not suggest refactors unless they fix a bug."

**Default Backend** — Optionally lock this agent to a specific backend. If set, the agent always uses this backend regardless of the workspace default. Leave it on "None" to inherit the workspace default.

**Include Agent Control** — When enabled, the agent gets access to Tyde's agent control MCP server and can spawn and manage sub-agents. This is how you create coordinator agents that delegate work.

## Scope

Definitions can be scoped globally or per-project. Global definitions are stored in `~/.tyde/agents/` and are available in all workspaces. Project definitions are stored in `.tyde/agents/` at the project root and are only available when that project is open. If a project definition has the same ID as a global one, the project version takes precedence.

Builtin definitions (like "Bridge") ship with Tyde and cannot be edited or deleted.

## Spawning an agent

Each definition in the agents list has a **Spawn** button. Click it to start a new conversation using that definition's settings. If you have multiple backends enabled, the dropdown arrow next to the button lets you choose which backend to use for this particular spawn.

You can also spawn agents from definitions through the chat backend picker or through the agent control MCP server when building multi-agent workflows.

## How instructions are delivered

The delivery mechanism varies by backend. Claude Code receives instructions through its native `--agents`/`--agent` CLI flags, which register the definition as a first-class agent identity. Codex receives them through its `model_instructions_file` configuration. Kiro and Gemini CLI receive instructions prepended to the message prompt. The effectiveness of custom instructions depends on the backend — Claude Code and Codex reliably follow them, while Kiro and Gemini may deprioritize them relative to their own built-in system prompts.
