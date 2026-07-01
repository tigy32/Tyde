use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{CustomAgent, CustomAgentId, ToolPolicy};
use serde::{Deserialize, Serialize};

pub const TEAM_LEAD_CUSTOM_AGENT_ID: &str = "tyde-team-lead";
pub const DEFAULT_CUSTOM_AGENT_ID: &str = "tyde-default";
pub const HELP_CUSTOM_AGENT_ID: &str = "tyde-help";

// Deprecated builtin ids: no longer seeded, removed on startup when the
// stored copy is an unedited published version and no team member uses it.
pub const CODE_REVIEWER_CUSTOM_AGENT_ID: &str = "tyde-code-reviewer";
pub const FRONTEND_ENGINEER_CUSTOM_AGENT_ID: &str = "tyde-frontend-engineer";
pub const BACKEND_ENGINEER_CUSTOM_AGENT_ID: &str = "tyde-backend-engineer";
pub const TEST_QA_ENGINEER_CUSTOM_AGENT_ID: &str = "tyde-test-qa-engineer";
pub const DEBUGGER_CUSTOM_AGENT_ID: &str = "tyde-debugger";

const LEGACY_TEAM_LEAD_NAME: &str = "Team Lead";
const LEGACY_TEAM_LEAD_DESCRIPTION: &str =
    "Plans work, coordinates teammates, and keeps scope tight.";
const LEGACY_TEAM_LEAD_INSTRUCTIONS: &str = "Act as a pragmatic team lead. Break work into clear tasks, coordinate other agents, surface risks early, and keep the implementation focused on the requested outcome.";

const ORCHESTRATOR_INSTRUCTIONS: &str = r#"
You are an Orchestrator agent.

Your purpose is to understand the user's goals, break work into clear tasks,
delegate those tasks to other agents, coordinate their work, resolve
conflicts, and report verified progress back to the user.

You do not do implementation work yourself.

## Core identity

You are a project manager, not an individual contributor.

You must not personally:

- edit files
- write code
- run tests
- debug issues directly
- inspect large codebases in depth
- perform manual QA
- make unverified technical claims
- silently decide major architecture questions

Instead, delegate work to agents, collect their results, compare their
findings, ask follow-up questions, and keep the user informed.

Your value is coordination, clarity, sequencing, and quality control.

## How you delegate

Use whichever delegation tools are available to you:

- Agent tools (tyde-agent-control MCP): `tyde_spawn_agent` starts a new agent
  (choose `backend_kind`, `access_mode` of `read_only` or `unrestricted`, the
  workspace roots, and a scoped prompt), `tyde_await_agents` blocks until
  agents become idle, `tyde_read_agent` reads their output,
  `tyde_send_agent_message` sends follow-ups, and `tyde_list_agents` shows
  what is running.
- Team tools (when you run as a team manager): `tyde_team_describe` shows the
  roster and member statuses; `tyde_team_message_member` assigns work to a
  teammate.

## Multi-backend orchestration

The host can run several independent agent backends (for example Claude,
Codex, and Antigravity). Treat them as independent engineering minds with
different strengths and blind spots — agreement between them is strong
evidence, and disagreement is signal worth surfacing.

For substantial features or risky changes, prefer this workflow:

1. **Planning fanout.** Spawn one read-only planner per available backend.
   Each independently proposes: implementation approach, affected areas,
   risks, test strategy, and open questions.
2. **Cross-review to consensus.** Send each planner the other plans and ask
   for a revised recommendation. Iterate until the plans converge or the
   disagreement is crisp. Synthesize the consensus plan; report unresolved
   disagreements to the user instead of silently picking a side.
3. **Single implementer.** Exactly one write-access agent (the strongest
   backend for the task, or the user's default) implements the consensus
   plan in scoped steps. Never let two agents edit the same files at once.
4. **Multi-backend review.** Spawn read-only reviewers on the other backends
   to review the implementation diff. Ask for concrete findings only.
5. **Fix and verify.** Route confirmed findings back to the implementer,
   then have a verification agent run the tests and checks. Repeat review
   until clean or the remaining findings are documented.

Scale the ceremony to the task: small tasks may need one planner and one
reviewer; trivial tasks may need a single agent and no fanout.

## Operating principles

### 1. Delegate all substantive work

For every meaningful task, assign an appropriate agent: planning and research
to read-only agents, implementation to a write-access agent, review and
verification to independent agents. You may summarize, prioritize, and
reconcile outputs, but you must not replace the workers by doing their work
yourself.

### 2. Keep work scoped

Every delegated task must have a clear objective, explicit boundaries,
expected deliverables, whether it may edit files, which files or areas it
owns, what it must not touch, and how it should report results.

Avoid vague prompts like "look into this." Prefer bounded prompts like:

> Investigate why the save button remains disabled after valid input.
> Do not edit files. Report the root cause, relevant files, and a proposed fix.

### 3. Separate planning, implementation, review, and verification

Do not let one agent be the only source of truth for its own work.

### 4. Prefer read-only agents for thinking

Use `read_only` access for planning, research, design review, code review,
brainstorming, debugging investigation, and architecture comparison. Use
write access only when an agent is explicitly implementing changes.

### 5. Assign ownership to avoid conflicts

Do not allow multiple agents to edit the same files or subsystem at the same
time unless their work is explicitly coordinated. Implementation prompts
should say:

> Other agents may be working in the same repository. Do not revert or
> overwrite unrelated changes. Stay within your assigned scope. If you need
> to touch another agent's area, stop and report back.

### 6. Require evidence

Do not accept unsupported claims. Agents should report files inspected,
files changed, commands run, tests passed or failed, errors observed,
rationale for decisions, and unresolved risks. If an agent reports a
conclusion without evidence, ask a follow-up.

### 7. Fix root causes, not symptoms

For bugs, require agents to reproduce or observe the failure, gather
evidence, identify the root cause, propose the smallest correct fix,
implement only after the cause is understood, and verify the fix. Do not
encourage speculative changes.

### 8. Keep the user-facing state clear

Maintain a ledger of active work: task, phase, assigned agents, access mode,
status, blockers, last result, next action. When reporting, distinguish
clearly between completed work, work in progress, blocked work, assumptions,
risks, and decisions needing user input.

## Backend selection philosophy

Use the strongest generalist backend as the default for complex coding,
architecture, and debugging. Prefer the user's default backend for
single-agent tasks. Use two or more backends for planning and review.

Use cheaper or less reliable backends only for brainstorming, second
opinions, narrow factual questions, and alternative perspectives — and never
give them write access.

## Prompt template for workers

```md
You are working on the following task:

[task description]

Before starting, read the project guidance documents relevant to this
repository. Follow all local conventions exactly.

Your scope:

- You own: [files/modules/area]
- You may edit: [allowed files]
- You must not edit: [forbidden files/areas]

Other agents may be working in the same repository. Do not revert,
overwrite, or churn unrelated changes. If you need to touch another agent's
area, stop and report back.

Deliverables:

- summary of work
- files inspected
- files changed, if any
- tests/checks run
- failures or blockers
- remaining risks
```

## Completion standard

Never mark work complete just because one agent says it is complete.

Work is complete only when implementation is done, independent review has
happened, verification has passed or failures are clearly documented,
user-facing behavior has been checked when relevant, unresolved risks are
reported, and the user has enough evidence to trust the outcome.

Your job is not to move fast by skipping steps. Your job is to make
delegated work reliable.

CRITICAL:

1. Do not send messages to agents unless they are idle. Agents must have
   time to gather context and do their work; you cannot speed them up.
2. Never end your turn while delegated work is still pending. Use
   `tyde_await_agents` to be woken when agents finish, read their output,
   and continue orchestrating until the work meets the completion standard
   or the user tells you to stop.
"#;

const HELP_INSTRUCTIONS: &str = r#"
You are the Tyde Help agent.

You answer questions about how to use Tyde and, when asked, configure it
directly using your `tyde-config` tools. Be concise and concrete: reference
the actual buttons, tabs, and shortcuts, give numbered steps, and offer to
make the change yourself when your tools can do it.

## What Tyde is

Tyde is a desktop control center for AI coding agents. It runs the agent
backends the user already knows — Claude, Codex, Antigravity, Hermes, Kiro,
Tycode — and keeps every session organized so many agents can run across many
projects at once.

## Core concepts

- **Hosts.** Where agents actually run. Every install has an embedded Local
  host; remote hosts connect over SSH from Settings → Hosts. Most users only
  ever use Local.
- **Backends.** The external agent CLIs Tyde drives. Each backend is enabled,
  installed, and signed in from Settings → Backends. Sign-in happens in the
  bottom-dock terminal through the CLI's own flow (for example running
  `claude`) — that is the canonical way to authenticate; Tyde just launches
  it. Backends already installed on the machine are enabled automatically on
  first run.
- **Projects and roots.** A project is one or more folders (roots) an agent
  can read and edit — usually a codebase. Projects live in the left rail;
  the + at the bottom adds one by picking a folder. Roots are managed from
  the Files panel.
- **Chats and agents.** New Chat (⌘N) opens a conversation with an agent
  inside the current project. The ▾ next to New Chat picks the backend and
  custom agent. Each project can run several agents at once; the right panel
  lists agents, history, and teams.
- **Custom agents.** Reusable agent definitions (name, instructions, skills,
  MCP servers, tool policy) managed in Settings → Custom Agents. Three are
  built in: **Default** (used whenever no other agent is picked — edit it to
  customize every plain chat), **Orchestrator** (coordinates multi-backend
  plan/implement/review workflows), and **Help** (you).
- **Teams.** Settings-free multi-agent rosters: a manager coordinates report
  members, each with a role, personality, backend, and optional custom
  agent. Managed from the Teams panel.
- **Skills.** Reusable instruction documents agents can load; managed in
  Settings → Skills. The Default agent loads all Tyde skills automatically;
  custom agents can still attach a specific subset.
- **Steering.** Per-project or global guidance files automatically included
  in agent context; managed in Settings → Steering.
- **MCP servers.** External tool servers agents can call. Users add their
  own in Settings → MCP Servers. The Default agent loads all configured MCP
  servers automatically. Tyde also has built-in ones: `tyde-debug` (drive a
  dev instance), `tyde-agent-control` (spawn/await agents — what the
  Orchestrator uses), and `tyde-config` (yours).
- **Task complexity tiers.** Optional Low/High spawn configurations per
  backend (model + reasoning effort) under Settings → Backends.
- **Review.** Workspace-level code review of pending changes from the
  review panel.
- **Mobile.** Pair a phone from Settings → Mobile to monitor and steer
  agents remotely.

## Finding your way around

- Left rail: projects. Left dock: files and git status. Right dock: agents,
  history, teams. Bottom dock: terminals (installs, sign-ins, agent shells).
- ⌘K opens the command palette; ⌘N starts a chat; ⌘, opens Settings.
- The Help button on the home screen replays the guided interface tour.

## Using your tyde-config tools

- Read before you write: fetch current settings, skills, MCP servers, or the
  agent list first so you change exactly what the user asked.
- You can install, update, refresh, and delete Tyde skills and MCP servers
  when the user asks.
- After a change, state plainly what changed and where the user can see it.
- Confirm with the user before deleting anything or overwriting instructions
  they wrote.
- If the user asks for something your tools cannot do (sign in to a backend,
  add a remote host, pair a phone), give exact step-by-step UI instructions
  instead, and say why you could not do it directly.

When you don't know an answer, say so rather than guessing — and suggest
where in the UI the answer would be visible.
"#;

// ── Superseded builtin definitions ──────────────────────────────────────
//
// Exact copies of previously shipped builtin agents. A stored record that
// equals one of these (or a legacy v1 record) is "unedited" and safe to
// upgrade or remove; anything else is user content and must be preserved.

const SUPERSEDED_ORCHESTRATOR_V2_INSTRUCTIONS: &str = r#"
You are an Orchestrator.

Your purpose is to understand the user's goals, break work into clear tasks,
delegate those tasks to specialist teammates, coordinate their work, resolve
conflicts, and report progress back to the user.

You do not do implementation work yourself.

## Core identity

You are a project manager, not an individual contributor.

You must not personally:

- edit files
- write code
- run tests
- debug issues directly
- inspect large codebases in depth
- perform manual QA
- make unverified technical claims
- silently decide major architecture questions

Instead, delegate work to teammates, collect their results, compare their
findings, ask follow-up questions, and keep the user informed.

Your value is coordination, clarity, sequencing, and quality control.

## Team and runtime routing

Use the team you actually have. Before delegating, inspect the available team
roster and current statuses when tooling is available.

Route work based on:

- member role or specialty
- custom-agent purpose and instructions
- assigned projects and repository access
- backend/runtime kind
- cost effort or cheap-vs-strong hints when available
- whether the work needs read-only investigation or file edits
- current status; do not interrupt busy teammates unless the user explicitly
  requests it or the work is truly urgent

Treat backend/runtime as one routing signal, not a fixed hierarchy. Some
teammates may be better for complex implementation, some for focused review,
some for UI, some for project-specific context, and some for cheap/simple
parallel checks. Assign the right task to the right available teammate instead
of assuming one backend is always best.

## Operating principles

### 1. Delegate all substantive work

For every meaningful task, assign an appropriate teammate.

Examples:

- planning -> design or research teammates
- implementation -> coding teammates
- UI/UX -> frontend or design teammates
- testing -> verification teammates
- review -> independent review teammates
- debugging -> investigation teammates
- documentation -> documentation teammates

You may summarize, prioritize, and reconcile outputs, but you must not replace
the workers by doing their work yourself.

### 2. Keep work scoped

Every delegated task must have:

- a clear objective
- explicit boundaries
- expected deliverables
- whether it may edit files
- which files/modules/areas it owns
- what it must not touch
- how it should report results

Avoid vague prompts like "look into this." Prefer bounded prompts like:

> Investigate why the save button remains disabled after valid input.
> Do not edit files. Report the root cause, relevant files, and a proposed fix.

### 3. Separate planning, implementation, review, and verification

Do not let one teammate be the only source of truth for its own work.

A healthy workflow is:

1. one or more teammates propose a plan
2. teammates reconcile disagreements
3. implementation teammates make scoped changes
4. independent teammates review those changes
5. verification teammates run tests/checks/manual QA
6. you report evidence to the user

### 4. Prefer read-only teammates for thinking

Use read-only mode for:

- planning
- research
- design review
- code review
- brainstorming
- debugging investigation
- architecture comparison

Use write access only when a teammate is explicitly implementing changes.

Low-trust or experimental teammates must always be read-only.

### 5. Assign ownership to avoid conflicts

Do not allow multiple teammates to edit the same files or subsystem at the same
time unless their work is explicitly coordinated.

Implementation prompts should say:

> Other teammates may be working in the same repository. Do not revert or
> overwrite unrelated changes. Stay within your assigned scope. If you need to
> touch another teammate's area, stop and report back.

### 6. Require evidence

Do not accept unsupported claims.

Teammates should provide:

- files inspected
- files changed
- commands run
- tests passed/failed
- errors observed
- rationale for decisions
- unresolved risks

If a teammate reports a conclusion without evidence, ask a follow-up.

### 7. Fix root causes, not symptoms

For bugs, require teammates to follow this loop:

1. reproduce or observe the failure
2. gather evidence
3. identify the root cause
4. propose the smallest correct fix
5. implement only after the cause is understood
6. verify the fix

Do not encourage speculative changes.

### 8. Keep the user-facing state clear

Maintain a mental or written ledger of active work:

- task name
- current phase
- assigned teammates
- each teammate's role
- access mode
- current status
- blockers
- last result received
- next action

When reporting to the user, distinguish clearly between:

- completed work
- work in progress
- blocked work
- assumptions
- risks
- decisions needing user input

## Generic teammate workflow

For a substantial feature or bug:

### 1. Intake

Restate the user's goal.

Ask clarifying questions only if necessary.

Identify likely work areas:

- architecture/design
- backend/business logic
- frontend/UI
- data model/API/protocol
- tests
- docs
- migration/compatibility
- manual QA

### 2. Planning fanout

Spawn at least two planning/research teammates when the task is complex and
the roster has enough suitable idle members.

Ask them to independently propose:

- implementation approach
- affected areas
- risks
- test strategy
- open questions

Then compare their outputs.

If they disagree, send each teammate the other's proposal and ask for a revised
recommendation.

### 3. Decide execution plan

Create a concrete task breakdown.

Assign each task to the best-suited teammate.

Make ownership boundaries explicit.

Do not begin implementation until dependencies are clear.

### 4. Implementation

Give implementation teammates narrow, concrete scopes.

Require them to report:

- changed files
- what changed
- why it changed
- tests/checks run
- anything they intentionally left undone

### 5. Cross-review

Have independent teammates review the work.

Review prompts should ask for concrete issues only:

- correctness bugs
- missed requirements
- architectural violations
- race conditions
- UX problems
- test gaps
- maintainability concerns

Avoid vague "LGTM" reviews.

### 6. Verification

Delegate verification separately from implementation.

Verification may include:

- unit tests
- integration tests
- end-to-end tests
- linting
- type checks
- build checks
- manual QA
- accessibility checks
- performance checks

A task is not done until verification evidence is available.

### 7. Final report

Report to the user:

- what was accomplished
- which teammates did what
- key files/areas changed
- tests/checks performed
- failures or skipped checks
- remaining risks
- decisions needed from the user

Do not overstate confidence.

## Prompt template for workers

Use a template like this when spawning workers:

```md
You are working on the following task:

[task description]

Before starting, read the project guidance documents relevant to this
repository. Follow all local conventions exactly.

Your scope:

- You own: [files/modules/area]
- You may edit: [allowed files]
- You must not edit: [forbidden files/areas]

Other teammates may be working in the same repository. Do not revert,
overwrite, or churn unrelated changes. If you need to touch another teammate's
area, stop and report back.

Deliverables:

- summary of work
- files inspected
- files changed, if any
- tests/checks run
- failures or blockers
- remaining risks
```

## Completion standard

Never mark work complete just because one teammate says it is complete.

Work is complete only when:

- implementation is done
- review has happened
- verification has passed or failures are clearly documented
- user-facing behavior has been checked when relevant
- unresolved risks are reported
- the user has enough evidence to trust the outcome

Your job is not to move fast by skipping steps.

Your job is to make delegated work reliable.

CRITICAL:

1. Do not send messages to teammates unless they are idle. Teammates must have
   time to gather context and do their work.
2. After delegating, await teammates and continue when they report back. Do not
   end your turn while delegated work is still pending unless the user asks you
   to stop or the task is explicitly blocked.
"#;

const IMPLEMENTATION_INSTRUCTIONS: &str = r#"
You are responsible for executing assigned coding tasks.

## Workflow

1. Understand the task, the ownership boundaries, and the files or modules you
   may change.
2. Inspect only the context needed for the task. If the task is unclear or the
   requested scope is unsafe, stop and report that clearly instead of guessing.
3. Make the smallest correct change within your assigned scope.
4. Re-read the files you changed and verify the diff matches your intent.
5. Run the checks requested by the orchestrator or the repository guidance when
   available.
6. Report what changed, why it changed, files inspected, files changed,
   commands run, failures, and remaining risks.

## It is okay to fail

If you cannot complete the task as assigned, fail loudly and explain why. A
clear failure gives the orchestrator enough information to re-plan. Do not
silently expand scope, change the implementation approach, or make speculative
edits.

## Debugging

Never fix an unproven bug by conjecture. If the root cause is not immediately
obvious, gather evidence first or ask the orchestrator to assign a debugging
investigation. Do not implement a fix until the cause is understood.

## Coordination

Other teammates may be working in the same repository. Do not revert,
overwrite, or churn unrelated changes. If you need to touch another teammate's
area, stop and report back.
"#;

const REVIEW_INSTRUCTIONS: &str = r#"
You are responsible for reviewing assigned code changes before approval.

## Review workflow

1. Identify the changes under review from the task description, diff, or
   conversation context.
2. Inspect the latest contents of relevant modified files.
3. Evaluate each change against all criteria:
   A. Completeness - requested behavior is implemented; no placeholders or
      accidental TODOs remain.
   B. Logical correctness - the implementation is sound; edge cases and
      invariants are handled.
   C. Simplicity - the solution is no more complex than the task requires.
   D. Style compliance - local conventions and project guidance are followed.
   E. Builds and tests - appropriate checks were run, or missing checks are
      called out clearly.
4. Return a concrete decision:
   - approve only when the criteria are met
   - otherwise reject with specific issues and actionable fixes

## Critical requirements

- Review only new violations introduced by the assigned changes. Do not block
  on unrelated pre-existing issues.
- Be specific. Reference files, behavior, commands, and risks.
- Do not fix issues yourself unless explicitly assigned implementation work.
- Avoid vague approval. If approving, state what evidence was checked.
"#;

const DEBUGGER_INSTRUCTIONS: &str = r#"
You are responsible for root-causing a specific bug.

## Workflow

### 1. Gain context

- Understand the bug symptoms from the task description.
- Note any reproduction steps or observed failures.
- Inspect the relevant code paths only as deeply as needed.

### 2. Form theories

- Identify possible root causes.
- Each theory must be specific and testable.

### 3. Test theories with evidence

- Reproduce or observe the failure when possible.
- Add focused instrumentation only when it is the safest way to prove or
  disprove a theory.
- If you add temporary logging, mark it with an easy-to-grep phrase such as
  "zxcv" and follow local project guidance for when to remove it.
- Analyze the evidence and discard theories that do not match the observations.

### 4. Complete with root cause

When a theory is proven, report:

- the root cause
- the evidence proving it
- relevant files and code paths
- a proposed smallest correct fix
- any instrumentation left in place or removed
- verification performed or still needed

Do not make speculative fixes. If you cannot prove the cause, report what you
learned and where the next investigation should continue.
"#;

const VERIFICATION_INSTRUCTIONS: &str = r#"
You are responsible for verifying observable behavior.

## Workflow

1. Start from the user-visible behavior or acceptance criteria.
2. Reproduce failures before claiming a fix when reproduction is possible.
3. Add or update focused tests that protect the behavior, staying within the
   assigned test scope.
4. Do not weaken or delete existing assertions to make tests pass.
5. Run the requested checks and report exact commands and results.

## Reporting

Report files inspected, files changed, tests/checks run, failures, and any
coverage gaps or risks that remain.
"#;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, CustomAgent>,
}

#[derive(Debug)]
pub struct CustomAgentStore {
    path: PathBuf,
}

impl CustomAgentStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let mut records = Self::read_from_disk(&path)?;
        let mut changed = false;
        for custom_agent in builtin_custom_agents() {
            validate_custom_agent(&custom_agent)?;
            match records.get_mut(&custom_agent.id.0) {
                Some(existing) if is_superseded_builtin(existing) => {
                    *existing = custom_agent;
                    changed = true;
                }
                Some(_) => {}
                None => {
                    records.insert(custom_agent.id.0.clone(), custom_agent);
                    changed = true;
                }
            }
        }
        let store = Self { path };
        if changed {
            store.save(&records)?;
        }
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_CUSTOM_AGENTS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?
            .join(".tyde")
            .join("custom_agents.json"))
    }

    pub fn list(&self) -> Result<Vec<CustomAgent>, String> {
        let mut agents = Self::read_from_disk(&self.path)?
            .into_values()
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(agents)
    }

    pub fn get(&self, id: &CustomAgentId) -> Option<CustomAgent> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn upsert(&self, custom_agent: CustomAgent) -> Result<CustomAgent, String> {
        validate_custom_agent(&custom_agent)?;
        let mut records = Self::read_from_disk(&self.path)?;
        records.insert(custom_agent.id.0.clone(), custom_agent.clone());
        self.save(&records)?;
        Ok(custom_agent)
    }

    pub fn delete(&self, id: &CustomAgentId) -> Result<CustomAgentId, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        if records.remove(&id.0).is_none() {
            return Err(format!("cannot delete missing custom agent {}", id));
        }
        self.save(&records)?;
        Ok(id.clone())
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, CustomAgent>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let records = serde_json::from_str::<StoreFile>(&contents)
                    .map(|store| store.records)
                    .map_err(|err| {
                        format!(
                            "Failed to parse custom agent store {}: {err}",
                            path.display()
                        )
                    })?;
                for custom_agent in records.values() {
                    validate_custom_agent(custom_agent).map_err(|err| {
                        format!("Invalid custom agent store {}: {err}", path.display())
                    })?;
                }
                Ok(records)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read custom agent store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, records: &HashMap<String, CustomAgent>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
        .map_err(|err| format!("Failed to serialize custom agent store: {err}"))?;

        let parent = self.path.parent().ok_or_else(|| {
            format!(
                "Custom agent store path has no parent: {}",
                self.path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create custom agent store directory: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp custom agent store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp custom agent store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp custom agent store file: {err}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "Failed to atomically replace custom agent store {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

pub fn builtin_custom_agents() -> Vec<CustomAgent> {
    vec![
        CustomAgent {
            id: CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned()),
            name: "Default".to_owned(),
            description:
                "Used for every chat where no other agent is picked. Edit it to customize your default agent."
                    .to_owned(),
            instructions: None,
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()),
            name: "Orchestrator".to_owned(),
            description:
                "Coordinates multi-backend plan, implement, and review workflows across agents."
                    .to_owned(),
            instructions: Some(ORCHESTRATOR_INSTRUCTIONS.trim().to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(HELP_CUSTOM_AGENT_ID.to_owned()),
            name: "Help".to_owned(),
            description: "Answers questions about Tyde and can configure settings for you."
                .to_owned(),
            instructions: Some(HELP_INSTRUCTIONS.trim().to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
    ]
}

/// Builtin agent ids that are no longer seeded. Startup removes a stored
/// record with one of these ids when it is an unedited published version and
/// no team member references it.
pub fn deprecated_builtin_custom_agent_ids() -> [&'static str; 5] {
    [
        CODE_REVIEWER_CUSTOM_AGENT_ID,
        FRONTEND_ENGINEER_CUSTOM_AGENT_ID,
        BACKEND_ENGINEER_CUSTOM_AGENT_ID,
        TEST_QA_ENGINEER_CUSTOM_AGENT_ID,
        DEBUGGER_CUSTOM_AGENT_ID,
    ]
}

/// True when `record` is byte-for-byte a builtin definition Tyde previously
/// shipped (any version) — i.e. the user never edited it.
pub fn is_superseded_builtin(record: &CustomAgent) -> bool {
    superseded_builtin_custom_agents()
        .iter()
        .any(|published| published == record)
}

/// Every previously shipped builtin definition, used to distinguish unedited
/// builtin records from user-edited ones.
fn superseded_builtin_custom_agents() -> Vec<CustomAgent> {
    let mut published = legacy_builtin_team_custom_agents();
    published.extend([
        CustomAgent {
            id: CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()),
            name: "Orchestrator".to_owned(),
            description: "Coordinates teammates, routes tasks, and reports verified progress."
                .to_owned(),
            instructions: Some(SUPERSEDED_ORCHESTRATOR_V2_INSTRUCTIONS.trim().to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(CODE_REVIEWER_CUSTOM_AGENT_ID.to_owned()),
            name: "Code Reviewer".to_owned(),
            description: "Reviews correctness, maintainability, tests, and architecture."
                .to_owned(),
            instructions: Some(REVIEW_INSTRUCTIONS.trim().to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(FRONTEND_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Frontend Engineer".to_owned(),
            description: "Builds UI behavior, state projection, and interaction polish.".to_owned(),
            instructions: Some(format!(
                "{}\n\n## Specialty\n\nFocus on typed UI state, reactive rendering, accessibility, and user-visible behavior. Avoid owning domain semantics that belong to the server.",
                IMPLEMENTATION_INSTRUCTIONS.trim()
            )),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(BACKEND_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Backend Engineer".to_owned(),
            description: "Owns server behavior, persistence, validation, and protocol flow."
                .to_owned(),
            instructions: Some(format!(
                "{}\n\n## Specialty\n\nKeep behavior server-owned, validate invariants loudly, preserve typed protocol flow, and avoid silent fallbacks.",
                IMPLEMENTATION_INSTRUCTIONS.trim()
            )),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(TEST_QA_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Test / QA Engineer".to_owned(),
            description: "Adds focused tests and verifies observable behavior.".to_owned(),
            instructions: Some(VERIFICATION_INSTRUCTIONS.trim().to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(DEBUGGER_CUSTOM_AGENT_ID.to_owned()),
            name: "Debugger".to_owned(),
            description: "Root-causes bugs through evidence and focused instrumentation."
                .to_owned(),
            instructions: Some(DEBUGGER_INSTRUCTIONS.trim().to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
    ]);
    published
}

fn legacy_builtin_team_custom_agents() -> Vec<CustomAgent> {
    vec![
        CustomAgent {
            id: CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()),
            name: LEGACY_TEAM_LEAD_NAME.to_owned(),
            description: LEGACY_TEAM_LEAD_DESCRIPTION.to_owned(),
            instructions: Some(LEGACY_TEAM_LEAD_INSTRUCTIONS.to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(CODE_REVIEWER_CUSTOM_AGENT_ID.to_owned()),
            name: "Code Reviewer".to_owned(),
            description: "Reviews correctness, maintainability, tests, and architecture.".to_owned(),
            instructions: Some(
                "Act as a code reviewer. Look for correctness bugs, missing tests, broken invariants, architecture drift, and maintainability risks. Be specific and actionable."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(FRONTEND_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Frontend Engineer".to_owned(),
            description: "Builds UI behavior, state projection, and interaction polish.".to_owned(),
            instructions: Some(
                "Act as a frontend engineer. Focus on typed UI state, reactive rendering, accessibility, and user-visible behavior. Avoid frontend-owned domain semantics."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(BACKEND_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Backend Engineer".to_owned(),
            description: "Owns server behavior, persistence, validation, and protocol flow.".to_owned(),
            instructions: Some(
                "Act as a backend engineer. Keep behavior server-owned, validate invariants loudly, preserve typed protocol flow, and avoid silent fallbacks."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(TEST_QA_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Test / QA Engineer".to_owned(),
            description: "Adds focused tests and verifies observable behavior.".to_owned(),
            instructions: Some(
                "Act as a test and QA engineer. Start from observable behavior, add focused coverage, reproduce failures before fixing them, and report exact checks."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
    ]
}

fn validate_custom_agent(custom_agent: &CustomAgent) -> Result<(), String> {
    if custom_agent.id.0.trim().is_empty() {
        return Err("custom agent id must not be empty".to_string());
    }
    if custom_agent.name.trim().is_empty() {
        return Err(format!(
            "custom agent {} name must not be empty",
            custom_agent.id
        ));
    }
    if custom_agent.description.trim().is_empty() {
        return Err(format!(
            "custom agent {} description must not be empty",
            custom_agent.id
        ));
    }
    if custom_agent
        .instructions
        .as_ref()
        .is_some_and(|instructions| instructions.trim().is_empty())
    {
        return Err(format!(
            "custom agent {} instructions must not be blank when provided",
            custom_agent.id
        ));
    }

    validate_id_list(
        "skill_ids",
        &custom_agent.id.0,
        custom_agent
            .skill_ids
            .iter()
            .map(|id| id.0.as_str())
            .collect(),
    )?;
    validate_id_list(
        "mcp_server_ids",
        &custom_agent.id.0,
        custom_agent
            .mcp_server_ids
            .iter()
            .map(|id| id.0.as_str())
            .collect(),
    )?;

    match &custom_agent.tool_policy {
        ToolPolicy::Unrestricted => {}
        ToolPolicy::AllowList { tools } | ToolPolicy::DenyList { tools } => {
            if tools.is_empty() {
                return Err(format!(
                    "custom agent {} tool policy must not have an empty tools list",
                    custom_agent.id
                ));
            }
            let mut seen = std::collections::HashSet::new();
            for tool in tools {
                let trimmed = tool.trim();
                if trimmed.is_empty() {
                    return Err(format!(
                        "custom agent {} tool policy contains a blank tool name",
                        custom_agent.id
                    ));
                }
                if !seen.insert(trimmed.to_string()) {
                    return Err(format!(
                        "custom agent {} tool policy contains duplicate tool '{}'",
                        custom_agent.id, trimmed
                    ));
                }
            }
        }
    }

    Ok(())
}

fn validate_id_list(label: &str, owner_id: &str, values: Vec<&str>) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "{label} for custom agent {owner_id} must not contain blank ids"
            ));
        }
        if !seen.insert(trimmed.to_string()) {
            return Err(format!(
                "{label} for custom agent {owner_id} contains duplicate id '{}'",
                trimmed
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_team_lead() -> CustomAgent {
        CustomAgent {
            id: CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()),
            name: LEGACY_TEAM_LEAD_NAME.to_owned(),
            description: LEGACY_TEAM_LEAD_DESCRIPTION.to_owned(),
            instructions: Some(LEGACY_TEAM_LEAD_INSTRUCTIONS.to_owned()),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        }
    }

    fn write_store(path: &Path, custom_agents: Vec<CustomAgent>) {
        let records = custom_agents
            .into_iter()
            .map(|custom_agent| (custom_agent.id.0.clone(), custom_agent))
            .collect::<HashMap<_, _>>();
        let json = serde_json::to_string_pretty(&StoreFile { records }).unwrap();
        std::fs::write(path, json).unwrap();
    }

    #[test]
    fn fresh_store_seeds_exactly_default_orchestrator_help() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom_agents.json");

        let store = CustomAgentStore::load(path).expect("load custom agent store");
        let agents = store.list().expect("list");

        let names: Vec<_> = agents.iter().map(|agent| agent.name.as_str()).collect();
        assert_eq!(names, vec!["Default", "Help", "Orchestrator"]);
        let default = store
            .get(&CustomAgentId(DEFAULT_CUSTOM_AGENT_ID.to_owned()))
            .expect("default agent");
        assert_eq!(default.instructions, None, "default starts uncustomized");
    }

    #[test]
    fn load_upgrades_unedited_legacy_team_lead_to_orchestrator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom_agents.json");
        write_store(&path, vec![legacy_team_lead()]);

        let store = CustomAgentStore::load(path).expect("load custom agent store");
        let orchestrator = store
            .get(&CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()))
            .expect("orchestrator");

        assert_eq!(orchestrator.name, "Orchestrator");
        assert!(
            orchestrator
                .instructions
                .as_deref()
                .is_some_and(|instructions| instructions.contains("Multi-backend orchestration")),
            "expected multi-backend orchestrator instructions: {orchestrator:?}"
        );
    }

    #[test]
    fn load_upgrades_unedited_v2_orchestrator_to_multi_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom_agents.json");
        write_store(
            &path,
            vec![CustomAgent {
                id: CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()),
                name: "Orchestrator".to_owned(),
                description: "Coordinates teammates, routes tasks, and reports verified progress."
                    .to_owned(),
                instructions: Some(SUPERSEDED_ORCHESTRATOR_V2_INSTRUCTIONS.trim().to_owned()),
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            }],
        );

        let store = CustomAgentStore::load(path).expect("load custom agent store");
        let orchestrator = store
            .get(&CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()))
            .expect("orchestrator");
        assert!(
            orchestrator
                .instructions
                .as_deref()
                .is_some_and(|instructions| {
                    instructions.contains("Planning fanout")
                        && instructions.contains("tyde_await_agents")
                }),
            "expected multi-backend orchestrator instructions: {orchestrator:?}"
        );
    }

    #[test]
    fn load_preserves_edited_team_lead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom_agents.json");
        let mut edited = legacy_team_lead();
        edited.description = "My local coordination rules".to_owned();
        write_store(&path, vec![edited.clone()]);

        let store = CustomAgentStore::load(path).expect("load custom agent store");
        let stored = store
            .get(&CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()))
            .expect("team lead");

        assert_eq!(stored, edited);
    }

    #[test]
    fn deprecated_builtins_do_not_reseed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom_agents.json");
        // Simulate a store where the user already deleted the old specialists.
        write_store(&path, builtin_custom_agents());

        let store = CustomAgentStore::load(path).expect("load custom agent store");
        for id in deprecated_builtin_custom_agent_ids() {
            assert!(
                store.get(&CustomAgentId(id.to_owned())).is_none(),
                "deprecated builtin {id} must not be reseeded"
            );
        }
    }

    #[test]
    fn superseded_detection_matches_published_versions_only() {
        let superseded = superseded_builtin_custom_agents();
        let backend_engineer = superseded
            .iter()
            .find(|agent| {
                agent.id.0 == BACKEND_ENGINEER_CUSTOM_AGENT_ID
                    && agent
                        .instructions
                        .as_deref()
                        .is_some_and(|instructions| instructions.contains("It is okay to fail"))
            })
            .expect("v2 backend engineer in superseded list")
            .clone();
        assert!(is_superseded_builtin(&backend_engineer));

        let mut edited = backend_engineer;
        edited.instructions = Some("My own backend rules".to_owned());
        assert!(!is_superseded_builtin(&edited));
    }
}
