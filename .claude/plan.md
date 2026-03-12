# Plan: Handle ConversationRegistered event & remove pending queue

## Scope: 2 files, ~8 modifications total (Low complexity — direct execution)

### 1. `src/bridge.ts`
- Add `ConversationRegisteredData` and `ConversationRegisteredPayload` interfaces
- Change `onChatEvent` signature: accept `onRegistered` callback + `onEvent` callback
- In the listener, check `raw.kind === "ConversationRegistered"` before calling `parseChatEvent`; dispatch to the appropriate callback

### 2. `src/app.ts`
- Update import to include `ConversationRegisteredPayload`
- Update `onChatEvent` call (line ~80) to pass two callbacks: `handleConversationRegistered` and `routeChatEvent`
- Add `handleConversationRegistered(payload)` method — resolves project from workspace_roots, finds workspace view, builds a `RuntimeAgent`, calls `view.syncRuntimeAgent(agent)`
- Add `resolveProjectForWorkspaceRoots(workspaceRoots)` helper — extracts workspace-root matching from `resolveProjectForRuntimeAgent`
- Delete `MAX_PENDING_CHAT_EVENTS_PER_CONVERSATION` constant
- Delete `pendingChatEventsByConversation` field
- Delete `enqueuePendingChatEvent` method
- Delete `flushPendingChatEvents` method
- Remove all `flushPendingChatEvents()` calls (in `getOrCreateWorkspaceView`, `applyRuntimeAgents`, `openRuntimeAgentInWorkspace`, `routeChatEvent`)
- Simplify `routeChatEvent` — no enqueueing, no refresh fallback; just `tryRouteChatEvent` or `console.error`

### Validation
- `npm run build` to verify TypeScript compiles
