# Workspace

When you open a project in Tyde, you get a workspace — a self-contained environment with its own chat tabs, file explorer, git panel, terminals, agents, and layout. Everything in a workspace is independent, so switching between projects doesn't interrupt running agents or lose your place.

<!-- TODO: screenshot of full workspace -->

## Layout

The workspace is divided into a center area and three dock zones: left, right, and bottom. The center area holds your chat and file tabs. The left zone defaults to the file explorer and git panel, the right zone to agents and sessions, and the bottom zone to the terminal.

You can drag widget tabs between zones to rearrange the layout, and resize panels by dragging the edges between them. Double-click a resize handle to reset it to its default size. Layout changes are saved per workspace.

Press **Ctrl+Shift+F** to enter full-screen chat mode, which hides all side and bottom panels to maximize the conversation view. Toggle panels back individually when you need them.

<!-- TODO: screenshot showing dock zones -->

## Chat

Chat tabs are where you interact with your coding agent. Type a message and press **Enter** to send it. Use **Shift+Enter** for a new line. Press **Escape** or click the interrupt button to cancel a response in progress.

You can attach images by clicking the paperclip button, pasting from your clipboard, or dragging files onto the input area. Supported formats are PNG, JPG, GIF, WebP, and SVG.

If you send a message while the agent is still responding, it gets queued. Queued messages appear above the input with options to steer (cancel the current response and send immediately) or remove them.

<!-- TODO: screenshot of chat with a conversation -->

## Tabs

Each workspace has its own tab bar. Chat tabs and file tabs share the same bar. Click **+** to create a new chat tab, or open files from the explorer or command palette.

Tabs show their state — a dot appears on inactive chat tabs with unread messages, and a streaming indicator shows when an agent is responding. Double-click a tab to rename it. Right-click for options like close, close others, or close all. Drag tabs to reorder them, or drag a chat tab out of the tab bar into a dock zone to pin it alongside your panels.

## File explorer

The file explorer in the left panel shows your project's directory tree. Click a folder to expand or collapse it, and click a file to open it in a tab. Use the search bar at the top to filter by name. Toggle hidden files (dotfiles) with the visibility button.

<!-- TODO: screenshot of file explorer -->

## Git

The git panel shows your current branch and organizes changes into three sections: staged, unstaged, and untracked. Each file has buttons to stage, unstage, view its diff, or discard changes. You can also stage or unstage all files in a section at once. Type a commit message at the bottom and click commit.

<!-- TODO: screenshot of git panel with some changes -->

## Diffs

When an agent modifies a file, the change appears as a diff in a new tab. Diffs support two view modes — unified (single pane with added and removed lines) and side-by-side (before and after columns). Both modes include syntax highlighting. You can expand context around each hunk or show the full file.

<!-- TODO: screenshot of a diff -->

## Terminal

The bottom panel hosts your terminals. Click **+** to create a new terminal session. Each terminal runs in your project directory and supports full ANSI rendering. You can run multiple terminals at the same time, each in its own tab.

## Agents

The agents panel in the right zone shows all running and completed agents. Each entry displays the agent's name, a summary of what it's doing, and its status. Sub-agents appear nested under their parent.

You can click an agent to jump to its conversation, interrupt it to stop its current operation, or terminate it entirely. Use the toolbar to hide inactive agents, hide sub-agents, or search by name.

<!-- TODO: screenshot of agents panel with a few agents -->

## Sessions

The sessions panel lists your previous conversations. Each entry shows a preview of the first message, the backend used, when it was created, and how many messages it contains. You can resume a session to pick up where you left off, rename it with a custom alias, export it, or delete it. Use the search bar to filter by name, workspace, or backend.

## Command palette

Press **Ctrl+K** to open the command palette. By default it searches files in your project with fuzzy matching. Type **>** to switch to command mode, which lists available actions with their keyboard shortcuts. Recent commands appear at the top.

## Keyboard shortcuts

Press **Ctrl+/** to see all available keyboard shortcuts. The most common ones:

**Ctrl+K** — Command palette. **Ctrl+N** — New conversation. **Ctrl+,** — Settings. **Ctrl+Shift+F** — Full-screen chat. **Ctrl+J** — Toggle task list. **Ctrl+B** — Toggle right panel. **Ctrl+L** — Clear chat.

## Multiple workspaces

The project rail on the far left shows all your open projects. Click a project to switch to its workspace. Each workspace maintains its own agents, tabs, terminals, and layout independently — agents keep running in the background when you switch away.
