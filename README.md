<p align="center"><img src="docs/assets/icon.png" width="64" alt="Tyde"></p>
<h1 align="center">Tyde — Agent Studio</h1>
<p align="center"><strong>Manage coding agents across projects, backends, and machines.</strong></p>
<p align="center"><a href="https://tycode.dev/tyde.html">Download Tyde</a> · <a href="docs/book.md">Tyde User Guide</a> · <a href="https://github.com/tigy32/Tyde/releases">Releases</a> · <a href="https://github.com/tigy32/Tyde/issues">Issues</a></p>

Tyde brings CLI coding agents into one workspace. Run agents concurrently,
organize work across repositories, review their changes, and coordinate agents
using different backends.

Start an agent locally without creating a project. Add projects when you want
code context. Connect your servers when you want agents to keep working after
your laptop disconnects.

[![Tyde showing three projects, an expanded file explorer, eight demo agents, and an active coding conversation](docs/assets/studio.png)](docs/assets/studio.png)

*Eight open demo agents across three projects, with the selected agent working on a search improvement. [How these images were captured](dev-docs/customer-documentation.md#product-captures).*

## Your agents. One workspace.

Use Claude, Codex, OpenCode, Grok, Antigravity, Hermes, and Kiro integrations
through a shared studio. Backend availability depends on the CLI setup on each
host; Tyde shows installation and readiness information in Settings → Backends.

Give independent tasks their own conversations. Keep agents working across
projects, find the ones that need your attention, and return to earlier work
through session history.

## Close your laptop. Keep work running.

Connect to your home server, work server, or both. Agents run on the selected
host and can continue when the desktop client disconnects. Reconnect to see the
host’s current workspace and pick up where you left off.

Your laptop can be the interface while your servers hold the projects and run
the agents. The remote hosts must remain running for their work to continue.

## Review the work where it happens.

Open projects with one or more repository roots. Browse files, inspect working
changes, and explore Git commits. Comment on files, diffs, and committed changes,
then submit precise feedback to an agent.

Use workbenches to give independent changes separate Git working trees. Follow
symbols and references with Rust and Python language-server integrations.

## Let different agents work together.

An agent can use Tyde’s tools to create subagents on other available backends.
Delegate an investigation, ask for an independent review, or coordinate several
bounded tasks. Follow the parent and its children in the same studio.

Define reusable agents with instructions, skills, MCP servers, and tool policy.
Manage those definitions on each host and reuse them across supported backends.
Individual harness capabilities and provider limits still apply.

## Get started

1. [Download and install Tyde](https://tycode.dev/tyde.html).
2. Open **Settings → Backends** on the local host and configure your CLI backend.
3. Start **New Chat**. You do not need a project for your first conversation.
4. Open a project to browse code and review changes, or add a remote host under
   **Settings → Hosts**.

## Read the user guide

Already installed Tyde? The user guide explains the interface and gives step-by-step instructions for its features:

- [Your first agent](docs/book.md#first-agent)
- [Projects and multiple roots](docs/book.md#projects)
- [Code review and Git](docs/book.md#review)
- [Concurrent agents](docs/book.md#concurrency) and [session history](docs/book.md#sessions)
- [Remote hosts](docs/book.md#remote)
- [Cross-backend delegation](docs/book.md#delegation)
- [Custom agents and skills](docs/book.md#agents-skills)
- [Workbenches](docs/book.md#workbenches)
- [Complete workflows](docs/book.md#workflows)
- [Settings](docs/book.md#settings) and [troubleshooting](docs/book.md#troubleshooting)

For the designed reading experience, open `docs/index.html` in a browser or serve
`docs/` with a static server. The links above open the repository edition of the book. See [documentation maintenance](dev-docs/customer-documentation.md)
for rendering and capture instructions.
