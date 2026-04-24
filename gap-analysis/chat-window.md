# Chat Window Gap Analysis

## Pass 8 - GPT-5 Codex - 2026-04-24

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
- `frontend/src/components/session_settings.rs`
- `frontend/src/components/tool_card.rs`
- `frontend/src/components/task_list.rs`
- `frontend/src/dispatch.rs`
- `frontend/src/app.rs`
- `protocol/src/types.rs`

### Implemented since earlier passes
- Chat now supports image attachment input (drag/drop), pre-send thumbnails, and image rendering in messages.
- Session settings bar exists in chat and is schema-driven from backend session settings.
- Context usage + tasks summary is now much richer (context/tasks toggle, legend, utilization bars, collapsed progress behavior).
- Queue/interrupt UX is present (queue rows, send-now/cancel, interrupt, steer).
- Retry/cancel events render as visible cards.
- Scroll behavior now includes scrolled-up detection and a scroll-to-bottom affordance.
- Chat markdown/rich rendering is restored; assistant cards include richer metadata and copy controls.
- Streaming tool cards are rendered while streaming.
- Chat links can open local files in the file viewer.
- `AgentError` now appends transcript-visible error messages.

### Remaining gaps vs legacy
- No message virtualization for long conversations.
- Prompt history recall/draft history (`ArrowUp`/`ArrowDown`) is still missing.
- No post-render image lightbox/zoom viewer.
- Queued messages cannot be edited in-place (cancel/send-now only).
- Tool events can still be orphaned in edge cases when a tool request arrives with no active stream and no prior message.
- No dedicated specialized UI for plan-review / ask-user-question tool flows (falls back to generic rendering).
- No explicit legacy-style profile switching surface in chat (session settings exist, but not full legacy profile/admin flows).
- No chat-tab auto-title/manual-title reconciliation behavior comparable to legacy tab-title workflows.
