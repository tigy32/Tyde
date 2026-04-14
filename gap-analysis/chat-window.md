# Chat Window Gap Analysis

## Pass 1 - GPT-5 Codex - 2026-04-13

### Legacy reference points
- `~/Tyde/src/chat.ts`
- `~/Tyde/src/chat/message_renderer.ts`
- `~/Tyde/src/chat/session_settings.ts`
- `~/Tyde/src/chat/tools.ts`
- `~/Tyde/src/chat/input.ts`
- `~/Tyde/src/tasks.ts`

### Rewrite reference points
- `frontend/src/components/chat_view.rs`
- `frontend/src/components/chat_message.rs`
- `frontend/src/components/chat_streaming.rs`
- `frontend/src/components/chat_input.rs`
- `frontend/src/components/tool_card.rs`
- `frontend/src/components/task_list.rs`
- `frontend/src/dispatch.rs`
- `protocol/src/types.rs`

### Legacy coverage
- Full conversation surface with virtualized message list, queueing, typing state, scroll-to-bottom handling, retry/relaunch states, and more resilient stream lifecycle handling.
- Session settings panel inside chat with profile/model/reasoning/autonomy/orchestration controls.
- Tool rendering was a major subsystem: pending tool cards during streaming, output verbosity modes, special rendering for diffs/commands/user-input/spawn tools, and better result summaries.
- Context usage and task list shared a summary panel that could switch between context and tasks.
- Image attachments were supported in both input and message rendering.
- Chat output could link back into file viewing/diff flows.

### Rewrite coverage
- Basic chat message list, streaming bubble, send box, tool cards, and task list card.
- Protocol already carries `reasoning`, `context_breakdown`, `images`, tool requests/results, retry attempts, and operation-cancelled events.
- The UI renders only a subset of that data.

### Confirmed gaps vs legacy
- No session settings panel in chat.
- No profile switching flow from chat.
- No model selection flow from chat.
- No autonomy/orchestration/reasoning controls.
- No image attachment input.
- No image rendering in chat messages even though `ChatMessage.images` exists in protocol.
- No context usage rendering even though `ChatMessage.context_breakdown` exists in protocol.
- Task rendering is much simpler than the legacy context/tasks summary panel.
- No queueing of user messages while an agent is busy.
- No explicit cancel/relaunch/retry card UX comparable to the legacy app.
- No scroll-to-bottom affordance or user-scrolled-up handling.
- No message virtualization for long conversations.
- No linked-file navigation from chat output.
- Tool rendering is materially reduced overall.
- No pending tool cards attached during stream.
- No verbosity mode toggle.
- No specialized rendering for spawn tools, user-input tools, or richer command/diff summaries.
- Tool requests are attached only to the last completed message in `dispatch.rs`; the legacy app maintained separate streaming/tool state, so streamed tool activity had a place to live before final message commit.
- No per-conversation settings refresh/update flows like the legacy `getSettings`, `listProfiles`, and `listModels` paths.

### Suggested next slices
- Bring back session settings and context/task summary before polishing message styling; those are major missing workflows, not cosmetic gaps.
- Fix tool event state handling before expanding tool UI, otherwise streamed tool calls will keep landing in the wrong place.
- Add image support after session settings, because the protocol and backend shape already account for it.

## Pass 2 - GPT-5 Codex - 2026-04-13

### Additional confirmed gaps
- Session-settings parity is blocked below the UI layer. The rewrite protocol does not define equivalents for legacy `SessionSettingsData`, `ProfilesListData`, or `ModelsListData`, so profile/model/settings parity is not implementable as a frontend-only task.
- Attachment parity is also blocked at protocol level. The rewrite `SendMessagePayload` only contains `message: String`, so the user cannot send images even though `ChatMessage` can carry `images` on output.
- Message rendering is substantially less capable. The legacy renderer uses richer content rendering and linked-file affordances; the rewrite renders message bodies as plain paragraph text.
- The rewrite input surface has no cancel button, no pending queue UI, and no "steer after current turn" flow comparable to the legacy chat panel.
- Tool-event handling is not just reduced, it is lossy. In `frontend/src/dispatch.rs`, `ToolRequest` is only attached if a prior message already exists for that agent; otherwise the request is silently dropped. `ToolExecutionCompleted` can then also fail to attach because the request was never recorded.
- Task support is partial rather than absent. The rewrite does render a task card, but it is downgraded relative to the legacy summary panel: no context/tasks toggle, no context legend, no mini utilization bar, and no collapsed progress treatment.
- Retry handling is also reduced. The protocol carries retry attempts, but the rewrite only appends transient notices; it does not recreate the legacy countdown card and cancel affordance.

### Architectural note
- Chat parity splits into two groups:
- Frontend rendering/state gaps: message virtualization, queueing, tool presentation, context usage, scroll affordances, richer content rendering.
- Protocol/control-plane gaps: session settings, profile/model lists, and image input.

## Pass 3 - GPT-5 Codex - 2026-04-13

### Interaction-level gaps
- Legacy chat renders message content through a richer content renderer, while the rewrite displays plain text paragraphs. That means markdown/rich-content parity is currently missing.
- Legacy assistant messages include copy controls, richer token badges, model/agent labeling, and better timestamp presentation. The rewrite shows a reduced subset.
- Legacy reasoning UI is more deliberate and token-aware. The rewrite only shows a basic `<details>` block.
- Legacy chat supports optimistic user echo handling. The rewrite appears to depend on server echo only.
- Legacy chat includes a visible typing indicator, queue indicator, and scroll-to-bottom button. The rewrite auto-scrolls but does not expose equivalent explicit UI.
- Legacy image attachments support thumbnail rendering before send and lightbox viewing after render. The rewrite has neither pre-send thumbnail UX nor post-render lightbox UX.
- Legacy retry/relaunch flows have dedicated cards and controls. The rewrite only surfaces retry/cancelled state as transient text notices.

## Pass 4 - GPT-5 Codex - 2026-04-13

### Test-backed behavior gaps
- Legacy E2E coverage explicitly tests queue lifecycle behavior: queue while typing, remove queued item, steer queued item, and interrupt button state transitions. The rewrite has none of this queue/interrupt UX.
- Legacy E2E coverage explicitly tests scroll behavior: auto-scroll, preserving user scroll position when scrolled up, showing a scroll-to-bottom button, and staying pinned through tool completion and stream end. The rewrite only has a simple auto-scroll effect.
- Legacy E2E coverage explicitly tests preservation of streamed reasoning when `StreamEnd` omits final reasoning payload. The rewrite stream handling is much simpler and does not document or test this edge case.
- Legacy E2E coverage exercises `AskUserQuestion` / plan-review style tool cards and waiting-for-response states. The rewrite tool UI has no specialized representation for these flows.
- Legacy E2E coverage verifies merged backend session behavior, replayed Claude tool cards, restored image attachments, and diff anchoring when reopening history. The rewrite chat/session model does not support these restored-history behaviors.
- Legacy E2E coverage verifies chat file links opening files with and without line numbers. The rewrite has no chat-to-file-link open flow.
- Legacy E2E coverage verifies auto-titling of chat tabs, preserving manual renames, and settling after stale updates. The rewrite has no comparable chat-tab/title system.

## Pass 5 - GPT-5 Codex - 2026-04-13

### Additional input and error-surface gaps
- Legacy chat input supports persisted prompt history with `ArrowUp` / `ArrowDown` recall and draft restoration via `InputHistory`. The rewrite `frontend/src/components/chat_input.rs` has no input-history model, so prompt recall is missing entirely.
- Legacy chat/workspace surfaces expose a real welcome state with a primary `New Chat` action before a conversation exists. The rewrite `ChatView` fallback is only a passive "Select an agent to start chatting" empty state and does not provide equivalent in-context start actions.
- Legacy chat error handling is transcript-visible. In the rewrite, `FrameKind::AgentError` in `frontend/src/dispatch.rs` only changes the agent card status; it does not append an inline chat/system error message to the active conversation.
