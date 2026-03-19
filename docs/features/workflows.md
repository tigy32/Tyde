# Workflows

A workflow is a reusable automation that chains together shell commands, agent tasks, and other workflows into a sequence of steps. Use workflows to automate repetitive prompting patterns — for example, pull and rebase, spawn an agent to resolve any conflicts, then spawn another agent to verify the tests pass, all with concurrent agents where it makes sense. You define the steps once, then run the workflow from a slash command in chat, the command palette, or the workflows panel. Tyde executes the steps in order, passing output from one step to the next, and shows progress in real time.

## Creating a workflow

Open the workflows panel in the right dock zone and click the gear icon, then **+ New Workflow**. The builder opens with four fields:

**Name** — A human-readable name for the workflow (e.g. "Git Pull & Rebase"). The ID and slash command are generated automatically from the name as you type, but you can edit the slash command manually.

**Description** — A brief summary of what the workflow does.

**Slash Command** — The trigger you'll type in chat to run this workflow (e.g. `/git-pull-rebase`). Must start with `/`.

**Scope** — Choose **Global** to make the workflow available in all workspaces, or **Project** to scope it to the current workspace. Global workflows are stored in `~/.tyde/workflows/`, project workflows in `.tyde/workflows/` at the project root. If a project workflow has the same ID as a global one, the project version takes precedence.

## Steps and actions

A workflow is made up of steps. Steps run sequentially — each step must complete before the next one starts. Within a step, you can add multiple actions, and all actions in a step run concurrently.

There are three action types:

**Run Command** — Execute a shell command in the workspace directory. Enter the command directly (e.g. `git pull --rebase`).

**Spawn Agent** — Start a coding agent with a prompt. The agent runs to completion and its final response becomes the action's output. While it's running, you can click the action card in the run detail to open the agent's conversation.

**Run Workflow** — Execute another workflow as a nested action. Select the target workflow from the list in the builder. The nested workflow's final output becomes this action's output.

## Output chaining

::: v-pre
Each step's combined output is available to the next step through the `{{previous_output}}` template variable. Use it in command strings or agent prompts to pass results forward. For example, a first step could run `git diff --name-only` and a second step could spawn an agent with the prompt "Review the following changed files: {{previous_output}}".
:::

## Running a workflow

There are three ways to run a workflow:

**Slash command** — Type the workflow's trigger (e.g. `/git-pull-rebase`) in the chat input. Tyde intercepts it, starts the workflow, and switches to the workflows panel to show progress.

**Command palette** — Press **Ctrl+K**, type **>**, and search for "Run Workflow" to see all available workflows.

**Workflows panel** — Click the dropdown button in the workflows panel toolbar and select a workflow from the list.

## Monitoring runs

The workflows panel shows all runs for the current session, sorted newest first. Each run card displays the workflow name, status, and elapsed time.

While a run is in progress, the card shows a spinner and the name of the current step. When it completes, the card auto-expands to show the full run detail — each step with its actions, their status, and their output. Failed runs show the step that failed and the error message.

Click a run card to expand or collapse its detail. Agent action cards are clickable and open the agent's conversation in a new tab.

Use **Hide completed** in the toolbar to filter out finished runs and focus on active ones.

## Managing workflows

Click the gear icon in the workflows panel toolbar to open the workflow manager. This lists all available workflows with their name and slash command. From here you can edit a workflow to update its steps, or delete it.

<!-- TODO: screenshot of workflow builder -->
