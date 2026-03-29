import { openWorkspace, sendPromptAndWaitForAssistant, sel } from './helpers';

const WORKSPACE_CONV_ID = 10000;

afterEach(async () => {
  await browser.execute(() => {
    delete (window as any).__mockFinalTypingDelayMs;
    delete (window as any).__mockReadFileContentByPath;
  });
});

async function emitChatEvent(conversationId: number, kind: string, data: unknown): Promise<void> {
  const payload = JSON.stringify({ conversationId, kind, data });
  await (browser as any).execute(function (json: string) {
    const parsed = JSON.parse(json);
    const listeners = (window as any).__test_listeners?.['chat-event'] || [];
    for (const h of listeners) {
      h({
        event: 'chat-event',
        id: 0,
        payload: {
          conversation_id: parsed.conversationId,
          event: { kind: parsed.kind, data: parsed.data },
        },
      });
    }
  }, payload);
}

async function spawnFeedbackAgent(filePath: string, lineContent: string, feedback: string): Promise<number> {
  const result = await browser.executeAsync(
    (
      path: string,
      line: string,
      note: string,
      done: (result?: { ok: true; id: number }) => void,
    ) => {
      const spawn = (window as any).__test_spawnFeedbackAgent as
        | ((p: string, l: string, n: string) => Promise<number>)
        | undefined;
      if (!spawn) {
        (done as any)({ ok: false, error: 'Missing __test_spawnFeedbackAgent hook' });
        return;
      }
      spawn(path, line, note)
        .then((id) => done({ ok: true, id }))
        .catch((err: unknown) => (done as any)({ ok: false, error: String(err) }));
    },
    filePath,
    lineContent,
    feedback,
  );

  const typedResult = result as { ok: true; id: number } | { ok: false; error: string } | null | undefined;
  if (!typedResult || typeof typedResult !== 'object' || !('ok' in typedResult)) {
    throw new Error(`Invalid feedback spawn result: ${String(typedResult)}`);
  }
  if (!typedResult.ok) {
    throw new Error(typedResult.error);
  }
  return typedResult.id;
}

async function openAgentsWidget(): Promise<void> {
  await browser.waitUntil(
    async () => (await (await $$(sel.dockWidgetTab)).length) > 0,
    { timeout: 5000, timeoutMsg: 'Expected dock widget tabs to be available' },
  );

  await browser.execute((tabSel: string) => {
    const tabs = Array.from(document.querySelectorAll(tabSel)) as HTMLElement[];
    const tab = tabs.find((el) => el.offsetParent !== null && (el.textContent ?? '').trim() === 'Agents');
    tab?.click();
  }, sel.dockWidgetTab);
}

async function openSessionsWidget(): Promise<void> {
  await browser.waitUntil(
    async () => (await (await $$(sel.dockWidgetTab)).length) > 0,
    { timeout: 5000, timeoutMsg: 'Expected dock widget tabs to be available' },
  );

  await browser.execute((tabSel: string) => {
    const tabs = Array.from(document.querySelectorAll(tabSel)) as HTMLElement[];
    const tab = tabs.find((el) => el.offsetParent !== null && (el.textContent ?? '').trim() === 'History');
    tab?.click();
  }, sel.dockWidgetTab);
}

async function clickVisibleSessionsRefresh(timeoutMsg: string): Promise<void> {
  await browser.waitUntil(
    async () => browser.execute((buttonSel: string) => {
      const buttons = Array.from(document.querySelectorAll(buttonSel)) as HTMLButtonElement[];
      const target = buttons.find((button) => {
        const style = window.getComputedStyle(button);
        const rect = button.getBoundingClientRect();
        return style.display !== 'none'
          && style.visibility !== 'hidden'
          && rect.width > 0
          && rect.height > 0
          && !button.disabled;
      });
      if (!target) return false;
      target.click();
      return true;
    }, sel.sessionsRefresh),
    { timeout: 5000, timeoutMsg },
  );
}

async function getVisibleSessionsPanelText(): Promise<string> {
  return browser.execute((panelSel: string) => {
    const panels = Array.from(document.querySelectorAll(panelSel)) as HTMLElement[];
    const panel = panels.find((node) => {
      const style = window.getComputedStyle(node);
      const rect = node.getBoundingClientRect();
      return style.display !== 'none'
        && style.visibility !== 'hidden'
        && rect.width > 0
        && rect.height > 0;
    });
    return panel?.textContent ?? '';
  }, sel.sessionsPanel);
}

async function setVisibleSessionsSearch(value: string): Promise<void> {
  await browser.waitUntil(
    async () => browser.execute((panelSel: string, searchValue: string) => {
      const panels = Array.from(document.querySelectorAll(panelSel)) as HTMLElement[];
      const panel = panels.find((node) => {
        const style = window.getComputedStyle(node);
        const rect = node.getBoundingClientRect();
        return style.display !== 'none'
          && style.visibility !== 'hidden'
          && rect.width > 0
          && rect.height > 0;
      });
      if (!panel) return false;
      const input = panel.querySelector('[aria-label="Search sessions"]') as HTMLInputElement | null;
      if (!input) return false;
      input.value = searchValue;
      input.dispatchEvent(new Event('input', { bubbles: true }));
      return true;
    }, sel.sessionsPanel, value),
    { timeout: 5000, timeoutMsg: 'Expected visible sessions search input' },
  );
}

async function getVisibleSessionCardTexts(): Promise<string[]> {
  return browser.execute((panelSel: string, cardSel: string) => {
    const panels = Array.from(document.querySelectorAll(panelSel)) as HTMLElement[];
    const panel = panels.find((node) => {
      const style = window.getComputedStyle(node);
      const rect = node.getBoundingClientRect();
      return style.display !== 'none'
        && style.visibility !== 'hidden'
        && rect.width > 0
        && rect.height > 0;
    });
    if (!panel) return [];
    const cards = Array.from(panel.querySelectorAll(cardSel)) as HTMLElement[];
    return cards.map((card) => card.textContent ?? '');
  }, sel.sessionsPanel, sel.sessionCard);
}

async function spawnRuntimeAgent(
  name: string,
  options?: { completionDelayMs?: number },
): Promise<{ agentId: number; conversationId: number }> {
  const result = await browser.executeAsync(
    (
      agentName: string,
      completionDelayMs: number | undefined,
      done: (value: { agent_id: number; conversation_id: number }) => void,
    ) => {
      (window as any).__TAURI_INTERNALS__.invoke('spawn_agent', {
        workspaceRoots: ['/mock/workspace'],
        prompt: 'Bridge runtime task',
        name: agentName,
        keepAliveWithoutTab: true,
        mockCompletionDelayMs: completionDelayMs,
      }).then(done);
    },
    name,
    options?.completionDelayMs,
  );

  return {
    agentId: result.agent_id,
    conversationId: result.conversation_id,
  };
}

function makeAssistantMessage(content: string, toolCalls: Array<{id: string, name: string, arguments: Record<string, unknown>}> = []): Record<string, unknown> {
  return {
    sender: { Assistant: { agent: 'tycode' } },
    content,
    timestamp: Date.now(),
    images: [],
    token_usage: null,
    reasoning: null,
    context_breakdown: null,
    tool_calls: toolCalls,
  };
}

describe('Message queue, steer, and interrupt', () => {
  it('comprehensive queue lifecycle: queue while typing → remove → drain → steer → button states', async () => {
    await openWorkspace();

    // Create chat tab
    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);
    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    // --- Cancel button starts disabled ---
    const cancelBtn = await $(sel.cancelBtn);
    await cancelBtn.waitForExist({ timeout: 3000 });
    expect(await cancelBtn.getAttribute('disabled')).not.toBeNull();
    expect(await cancelBtn.getText()).toBe('Interrupt');

    // --- Send initial message so we have a conversation ---
    await sendPromptAndWaitForAssistant('Initial message');

    // --- Cancel button still disabled after response ---
    expect(await cancelBtn.getAttribute('disabled')).not.toBeNull();
    await browser.execute(() => {
      (window as any).__mockFinalTypingDelayMs = 400;
    });

    // --- Emit TypingStatusChanged: true → cancel enables, shows "Interrupt" ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', true);
    await browser.waitUntil(
      async () => (await cancelBtn.getAttribute('disabled')) === null,
      { timeout: 3000, timeoutMsg: 'Cancel button should be enabled when typing' },
    );
    expect(await cancelBtn.getText()).toBe('Interrupt');

    // --- Type text in textarea → cancel changes to "Steer" ---
    await browser.execute(
      (el: HTMLElement) => {
        const ta = el as HTMLTextAreaElement;
        ta.value = 'some text';
        ta.dispatchEvent(new Event('input', { bubbles: true }));
      },
      input,
    );
    await browser.waitUntil(
      async () => (await cancelBtn.getText()) === 'Steer',
      { timeout: 3000, timeoutMsg: 'Cancel button should show Steer when typing + text' },
    );

    // --- Send message while AI is typing → queues locally ---
    const sendBtn = await $(sel.sendBtn);
    await sendBtn.waitForEnabled({ timeout: 3000 });
    await browser.execute((el: HTMLElement) => (el as HTMLButtonElement).click(), sendBtn);

    const queueIndicator = await $(sel.queueIndicator);
    await browser.waitUntil(
      async () => !(await queueIndicator.getAttribute('class'))?.includes('hidden'),
      { timeout: 3000, timeoutMsg: 'Queue indicator should be visible after queuing' },
    );

    // Verify queue item shows the message text
    const items1 = await $$(sel.queueItem);
    expect(items1.length).toBe(1);
    const itemText1 = await $(sel.queueItemText);
    expect(await itemText1.getText()).toBe('some text');

    // --- Queue a second message ---
    await browser.execute(
      (el: HTMLElement) => {
        const ta = el as HTMLTextAreaElement;
        ta.value = 'second queued';
        ta.dispatchEvent(new Event('input', { bubbles: true }));
      },
      input,
    );
    await browser.execute((el: HTMLElement) => (el as HTMLButtonElement).click(), sendBtn);

    await browser.waitUntil(
      async () => (await (await $$(sel.queueItem)).length) === 2,
      { timeout: 3000, timeoutMsg: 'Should have 2 queue items' },
    );

    // --- Remove first item via × button ---
    const removeButtons = await $$(sel.queueItemRemove);
    expect(removeButtons.length).toBe(2);
    await browser.execute((el: HTMLElement) => el.click(), removeButtons[0]);

    await browser.waitUntil(
      async () => (await (await $$(sel.queueItem)).length) === 1,
      { timeout: 3000, timeoutMsg: 'Should have 1 queue item after remove' },
    );

    // Remaining item should be "second queued"
    const remainingText = await $(sel.queueItemText);
    expect(await remainingText.getText()).toBe('second queued');

    // --- Steer button exists on remaining item ---
    const steerBtns = await $$(sel.queueItemSteer);
    expect(steerBtns.length).toBe(1);

    // --- TypingStatusChanged: false → drains one queued message ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);

    await browser.waitUntil(
      async () => (await queueIndicator.getAttribute('class'))?.includes('hidden') ?? false,
      { timeout: 3000, timeoutMsg: 'Queue indicator should hide after drain' },
    );

    // Cancel button should be disabled now (model idle)
    await browser.waitUntil(
      async () => (await cancelBtn.getAttribute('disabled')) !== null,
      { timeout: 3000, timeoutMsg: 'Cancel button should be disabled when idle' },
    );

    // --- Steer from queue: queue messages, click steer button ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', true);

    // Queue two messages
    await browser.execute(
      (el: HTMLElement) => {
        const ta = el as HTMLTextAreaElement;
        ta.value = 'msg A';
        ta.dispatchEvent(new Event('input', { bubbles: true }));
      },
      input,
    );
    await browser.execute((el: HTMLElement) => (el as HTMLButtonElement).click(), sendBtn);

    await browser.execute(
      (el: HTMLElement) => {
        const ta = el as HTMLTextAreaElement;
        ta.value = 'msg B';
        ta.dispatchEvent(new Event('input', { bubbles: true }));
      },
      input,
    );
    await browser.execute((el: HTMLElement) => (el as HTMLButtonElement).click(), sendBtn);

    await browser.waitUntil(
      async () => (await (await $$(sel.queueItem)).length) === 2,
      { timeout: 3000, timeoutMsg: 'Should have 2 items for steer test' },
    );

    // Click steer (↑) on second item — should remove it from queue and set pendingSteer
    const steerButtons = await $$(sel.queueItemSteer);
    await browser.execute((el: HTMLElement) => el.click(), steerButtons[1]);

    // Queue should now have 1 item (msg A remains)
    await browser.waitUntil(
      async () => (await (await $$(sel.queueItem)).length) === 1,
      { timeout: 3000, timeoutMsg: 'Steered item should be removed from queue' },
    );
    const steerRemaining = await $(sel.queueItemText);
    expect(await steerRemaining.getText()).toBe('msg A');

    // When typing goes false, pendingSteer (msg B) drains instead of queue
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);

    // msg A should still be in queue (steer took priority)
    const queueAfterSteer = await $$(sel.queueItem);
    expect(queueAfterSteer.length).toBe(1);
    expect(await (await $(sel.queueItemText)).getText()).toBe('msg A');

    // --- Steer from textarea: type text + click Steer button ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', true);

    await browser.execute(
      (el: HTMLElement) => {
        const ta = el as HTMLTextAreaElement;
        ta.value = 'steer text';
        ta.dispatchEvent(new Event('input', { bubbles: true }));
      },
      input,
    );

    // Button should show "Steer"
    await browser.waitUntil(
      async () => (await cancelBtn.getText()) === 'Steer',
      { timeout: 3000, timeoutMsg: 'Cancel should show Steer with text + typing' },
    );

    // Click Steer — captures text as pendingSteer, clears input
    await browser.execute((el: HTMLElement) => el.click(), cancelBtn);

    // Textarea should be cleared
    const textareaVal = await browser.execute(
      (el: HTMLElement) => (el as HTMLTextAreaElement).value,
      input,
    );
    expect(textareaVal).toBe('');

    // msg A still in queue (steer doesn't drain queue)
    const queueAfterTextSteer = await $$(sel.queueItem);
    expect(queueAfterTextSteer.length).toBe(1);

    // When typing stops, pendingSteer drains (not queue)
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);

    // msg A should still be in queue
    const finalQueue = await $$(sel.queueItem);
    expect(finalQueue.length).toBe(1);
    expect(await (await $(sel.queueItemText)).getText()).toBe('msg A');

    // Drain remaining queue item
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', true);
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);

    await browser.waitUntil(
      async () => (await queueIndicator.getAttribute('class'))?.includes('hidden') ?? false,
      { timeout: 3000, timeoutMsg: 'Queue should be empty after final drain' },
    );
  });
});

describe('Chat scroll behavior', () => {
  it('comprehensive scroll lifecycle: auto-scroll → scroll up → button → new messages → tool completion → stream end', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);
    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    const scrollBtn = await $(sel.scrollToBottom);
    const chatContainer = await $(sel.chatContainer);

    // Helper: get scroll state from the container
    const getScrollState = async () => browser.execute((s: string) => {
      const el = document.querySelector(s) as HTMLElement;
      if (!el) return { scrollTop: 0, scrollHeight: 0, clientHeight: 0, atBottom: true };
      const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 50;
      return { scrollTop: el.scrollTop, scrollHeight: el.scrollHeight, clientHeight: el.clientHeight, atBottom };
    }, sel.chatContainer);

    // --- Scroll button starts hidden (no overflow) ---
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');

    // --- Fill container with enough messages to cause overflow ---
    for (let i = 0; i < 20; i++) {
      await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
        sender: { Assistant: { agent: 'tycode' } },
        content: `Filler message ${i} with enough text to take up vertical space in the chat container.\n\nThis is a second paragraph to make each message taller and ensure we get overflow sooner.`,
        timestamp: Date.now(),
        images: [],
        token_usage: null,
        reasoning: null,
        context_breakdown: null,
        tool_calls: [],
      });
    }

    // Wait for messages to render and container to overflow
    await browser.waitUntil(
      async () => {
        const state = await getScrollState();
        return state.scrollHeight > state.clientHeight;
      },
      { timeout: 5000, timeoutMsg: 'Chat container should overflow after 20 messages' },
    );

    // --- Auto-scroll should keep us at the bottom ---
    const stateAfterFill = await getScrollState();
    expect(stateAfterFill.atBottom).toBe(true);

    // Scroll button should still be hidden (we're at bottom)
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');

    // --- Scroll up manually → button appears ---
    // Pin scrollTop=0 and dispatch inside waitUntil so the virtualizer cannot
    // fight the scroll position between polls under concurrent load.
    await browser.waitUntil(
      async () => {
        await browser.execute((s: string) => {
          const el = document.querySelector(s) as HTMLElement;
          if (el.scrollTop !== 0) {
            el.scrollTop = 0;
            el.dispatchEvent(new Event('scroll'));
          }
        }, sel.chatContainer);
        return !(await scrollBtn.getAttribute('class'))?.includes('hidden');
      },
      { timeout: 5000, timeoutMsg: 'Scroll-to-bottom button should appear when scrolled up' },
    );

    const stateScrolledUp = await getScrollState();
    expect(stateScrolledUp.atBottom).toBe(false);

    // --- New message while scrolled up: scroll stays put, button remains ---
    const scrollTopBefore = (await getScrollState()).scrollTop;

    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      sender: { Assistant: { agent: 'tycode' } },
      content: 'New message while user is scrolled up — should not move scroll position.',
      timestamp: Date.now(),
      images: [],
      token_usage: null,
      reasoning: null,
      context_breakdown: null,
      tool_calls: [],
    });

    // Small wait for any async scroll to settle
    await browser.pause(200);

    const scrollTopAfter = (await getScrollState()).scrollTop;
    expect(scrollTopAfter).toBe(scrollTopBefore);

    // Button should still be visible
    expect(await scrollBtn.getAttribute('class')).not.toContain('hidden');

    // --- Click scroll-to-bottom button → scrolls to bottom, button hides ---
    await browser.execute((el: HTMLElement) => el.click(), scrollBtn);

    await browser.waitUntil(
      async () => (await getScrollState()).atBottom,
      { timeout: 3000, timeoutMsg: 'Should be at bottom after clicking scroll-to-bottom' },
    );

    await browser.waitUntil(
      async () => (await scrollBtn.getAttribute('class'))?.includes('hidden') ?? false,
      { timeout: 3000, timeoutMsg: 'Scroll button should hide after clicking it' },
    );

    // --- New message at bottom → stays at bottom (the core fix) ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      sender: { Assistant: { agent: 'tycode' } },
      content: 'Message added while at bottom — scroll should follow.',
      timestamp: Date.now(),
      images: [],
      token_usage: null,
      reasoning: null,
      context_breakdown: null,
      tool_calls: [],
    });

    // Wait for rAF-based scroll to settle
    await browser.pause(100);

    const stateAfterNewMsg = await getScrollState();
    expect(stateAfterNewMsg.atBottom).toBe(true);
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');

    // --- Stream start/delta/end keeps scroll at bottom ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Streaming content that grows the container. ' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'More streaming content to ensure height increases. ' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Even more content.\n\nWith paragraphs.' });

    await browser.pause(100);
    const stateAfterDeltas = await getScrollState();
    expect(stateAfterDeltas.atBottom).toBe(true);

    // StreamEnd finalizes — scroll should remain at bottom
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Streaming content that grows the container. More streaming content to ensure height increases. Even more content.\n\nWith paragraphs.'),
    });

    await browser.pause(200);
    const stateAfterStreamEnd = await getScrollState();
    expect(stateAfterStreamEnd.atBottom).toBe(true);
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');

    // --- Tool request + completion keeps scroll at bottom ---
    const toolCallId = 'scroll-test-tool';
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Running a tool now.' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Running a tool now.', [
        { id: toolCallId, name: 'ReadFiles', arguments: { file_paths: ['/test.ts'] } },
      ]),
    });

    await browser.pause(100);
    expect((await getScrollState()).atBottom).toBe(true);

    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: toolCallId,
      tool_name: 'ReadFiles',
      tool_type: { kind: 'ReadFiles', file_paths: ['/test.ts'] },
    });

    await browser.pause(100);
    expect((await getScrollState()).atBottom).toBe(true);

    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: toolCallId,
      tool_name: 'ReadFiles',
      tool_result: { kind: 'ReadFiles', files: [{ path: '/test.ts', bytes: 500 }] },
      success: true,
    });

    await browser.pause(200);
    const stateAfterTool = await getScrollState();
    expect(stateAfterTool.atBottom).toBe(true);
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');

    // --- TypingStatusChanged true/false doesn't break scroll position ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', true);
    await browser.pause(100);
    expect((await getScrollState()).atBottom).toBe(true);

    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);
    await browser.pause(100);
    expect((await getScrollState()).atBottom).toBe(true);
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');

    // --- Off-tab updates: if tab was pinned to bottom, returning keeps it pinned ---
    await browser.keys(['Control', 'n']);
    await browser.waitUntil(
      async () => (await (await $$(sel.convTab)).length) >= 2,
      { timeout: 3000, timeoutMsg: 'Second chat tab should open' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      sender: { Assistant: { agent: 'tycode' } },
      content: 'Message added while this chat tab is inactive — should still be at bottom when reopened.',
      timestamp: Date.now(),
      images: [],
      token_usage: null,
      reasoning: null,
      context_breakdown: null,
      tool_calls: [],
    });

    await browser.execute((tabSel: string) => {
      const tabs = Array.from(document.querySelectorAll(tabSel)) as HTMLElement[];
      tabs[0]?.click();
    }, sel.convTab);

    await browser.waitUntil(
      async () => (await getScrollState()).atBottom,
      { timeout: 3000, timeoutMsg: 'Should be at bottom after switching back to tab with new message' },
    );
    expect(await scrollBtn.getAttribute('class')).toContain('hidden');
  });
});

describe('Reasoning rendering', () => {
  it('preserves streamed reasoning when StreamEnd omits the final reasoning payload', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'claude', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamReasoningDelta', {
      message_id: 'claude-reasoning-1',
      text: 'Checking workspace constraints before editing.',
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', {
      message_id: 'claude-reasoning-1',
      text: 'Answer ready.',
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: {
        timestamp: Date.now(),
        sender: { Assistant: { agent: 'claude' } },
        content: 'Answer ready.',
        reasoning: null,
        tool_calls: [],
        model_info: { model: 'claude-opus-4-6' },
        token_usage: null,
        context_breakdown: null,
        images: [],
      },
    });

    await browser.waitUntil(
      async () => browser.execute((assistantSel: string) => {
        const messages = Array.from(document.querySelectorAll(assistantSel)) as HTMLElement[];
        if (messages.length === 0) return false;
        const last = messages[messages.length - 1];
        const text = last.textContent ?? '';
        return text.includes('Checking workspace constraints before editing.')
          && text.includes('Answer ready.');
      }, sel.assistantMessage),
      { timeout: 5000, timeoutMsg: 'Expected streamed reasoning fallback to render in final assistant message' },
    );
  });
});

describe('Chat tab lifecycle', () => {
  it('comprehensive chat flow: welcome → messages → context → tools → sessions → error handling', async () => {
    // --- Welcome screen with no chat input ---
    await openWorkspace();

    const welcome = await $(sel.welcomeScreen);
    await welcome.waitForExist({ timeout: 5000 });
    expect(await welcome.isDisplayed()).toBe(true);

    const textareas = await $$(sel.messageInput);
    const visibleTextareas = [];
    for (const ta of textareas) {
      if (await ta.isDisplayed()) visibleTextareas.push(ta);
    }
    expect(visibleTextareas.length).toBe(0);

    // --- Create new chat via welcome button ---
    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });
    expect(await input.isDisplayed()).toBe(true);

    const sendBtn = await $(sel.sendBtn);
    expect(await sendBtn.isExisting()).toBe(true);

    // --- Send first message and verify response ---
    await sendPromptAndWaitForAssistant('Hello');

    const hasAssistant = await browser.execute((s: string) => {
      const msgs = document.querySelectorAll(s);
      if (msgs.length === 0) return false;
      const last = msgs[msgs.length - 1];
      return (last.textContent?.length ?? 0) > 0;
    }, sel.assistantMessage);
    expect(hasAssistant).toBe(true);

    // --- Context bar updates after first response ---
    const taskPanel = await $(sel.taskPanel);
    await taskPanel.waitForExist({ timeout: 5000 });
    expect(await taskPanel.isDisplayed()).toBe(true);

    const usageEl = await $(sel.contextUsage);
    await browser.waitUntil(
      async () => {
        const text = await usageEl.getText();
        return text.includes('50.0K');
      },
      { timeout: 5000, timeoutMsg: 'Context usage should show 50.0K' },
    );
    let usageText = await usageEl.getText();
    expect(usageText).toContain('200.0K');
    expect(usageText).toContain('25.0%');

    // --- Second message updates context bar ---
    await sendPromptAndWaitForAssistant('Second message');

    await browser.waitUntil(
      async () => {
        const text = await usageEl.getText();
        return text.includes('100.0K');
      },
      { timeout: 5000, timeoutMsg: 'Context usage should update to 100.0K' },
    );
    usageText = await usageEl.getText();
    expect(usageText).toContain('50.0%');

    const bar = await $(sel.contextBar);
    await bar.waitForExist({ timeout: 5000 });
    const segments = await bar.$$(sel.contextSegment);
    expect(segments.length).toBeGreaterThan(0);

    // Regression: context bar utilization must reflect actual usage, not 100%
    const ariaValue = await bar.getAttribute('aria-valuenow');
    expect(Number(ariaValue)).toBeLessThanOrEqual(60);

    // Regression: each segment width must be a valid number ≤ 100%
    const segmentWidths: number[] = [];
    for (const seg of segments) {
      const style = await seg.getAttribute('style');
      const match = style?.match(/width:\s*([\d.]+)%/);
      expect(match).not.toBeNull();
      const width = parseFloat(match![1]);
      expect(width).not.toBeNaN();
      expect(width).toBeGreaterThan(0);
      expect(width).toBeLessThanOrEqual(100);
      segmentWidths.push(width);
    }

    const totalSegmentWidth = segmentWidths.reduce((a, b) => a + b, 0);
    expect(totalSegmentWidth).toBeLessThanOrEqual(100);

    // --- Multiple messages accumulate ---
    const countBefore = await browser.execute(
      (s: string) => document.querySelectorAll(s).length,
      sel.assistantMessage,
    );
    await sendPromptAndWaitForAssistant('Third message');
    const countAfter = await browser.execute(
      (s: string) => document.querySelectorAll(s).length,
      sel.assistantMessage,
    );
    expect(Number(countAfter)).toBeGreaterThan(Number(countBefore));

    // --- Streaming indicator shows during stream and finalizes ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Partial ' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'response' });

    const streamingBubble = await $(sel.streamingMessage);
    await streamingBubble.waitForExist({ timeout: 3000 });

    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Partial response'),
    });

    await browser.waitUntil(
      async () => {
        const count = await browser.execute(
          (s: string) => document.querySelectorAll(s).length,
          sel.streamingMessage,
        );
        return Number(count) === 0;
      },
      { timeout: 5000, timeoutMsg: 'Streaming bubble should finalize after StreamEnd' },
    );

    // --- Typing indicator shows and hides ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', true);

    const indicator = await $(sel.typingIndicator);
    await browser.waitUntil(
      async () => {
        const display = await indicator.getCSSProperty('display');
        return display.value !== 'none';
      },
      { timeout: 3000, timeoutMsg: 'Typing indicator should show' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);

    await browser.waitUntil(
      async () => {
        const display = await indicator.getCSSProperty('display');
        return display.value === 'none';
      },
      { timeout: 3000, timeoutMsg: 'Typing indicator should hide' },
    );

    // --- Event isolation: unknown conversation events don't leak ---
    await emitChatEvent(99999, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(99999, 'StreamDelta', { text: 'Ghost message' });
    await emitChatEvent(99999, 'StreamEnd', { message: makeAssistantMessage('Ghost message') });

    const hasGhost = await browser.execute((s: string) => {
      const container = document.querySelector(s);
      return container?.textContent?.includes('Ghost message') ?? false;
    }, sel.chatContainer);
    expect(hasGhost).toBe(false);

    // --- Error event renders as error message ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'Error', 'Something went wrong');

    const errorMsg = await $(sel.errorMessage);
    await browser.waitUntil(
      async () => {
        const text = await browser.execute(
          (s: string) => document.querySelector(s)?.textContent ?? '',
          sel.errorMessage,
        );
        return text.includes('Something went wrong');
      },
      { timeout: 5000, timeoutMsg: 'Error message should contain "Something went wrong"' },
    );

    // --- Tool calls: pending → running → done/failed lifecycle ---
    const toolCallId1 = 'test-tool-1';
    const toolCallId2 = 'test-tool-2';

    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'I will use two tools.' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('I will use two tools.', [
        { id: toolCallId1, name: 'ReadFiles', arguments: { file_paths: ['/mock/file.ts'] } },
        { id: toolCallId2, name: 'ModifyFile', arguments: { file_path: '/mock/broken.ts' } },
      ]),
    });

    // Pending cards should appear immediately
    await browser.waitUntil(
      async () => browser.execute(
        (s: string) => document.querySelectorAll(s).length >= 2,
        sel.toolCard,
      ),
      { timeout: 5000, timeoutMsg: 'Two pending tool cards should appear after StreamEnd' },
    );

    // Both cards should show "Pending" status
    const pendingStatuses = await browser.execute((statusSel: string) => {
      const elements = document.querySelectorAll(statusSel);
      return Array.from(elements).slice(-2).map(el => el.textContent?.trim());
    }, sel.toolStatusText);
    expect(pendingStatuses).toEqual(['Pending', 'Pending']);

    // First tool starts running
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: toolCallId1,
      tool_name: 'ReadFiles',
      tool_type: { kind: 'ReadFiles', file_paths: ['/mock/file.ts'] },
    });

    await browser.waitUntil(
      async () => {
        const statuses = await browser.execute((statusSel: string) => {
          const elements = document.querySelectorAll(statusSel);
          return Array.from(elements).slice(-2).map(el => el.textContent?.trim());
        }, sel.toolStatusText);
        return statuses[0] === 'Running...';
      },
      { timeout: 3000, timeoutMsg: 'First tool should transition to Running...' },
    );

    // First tool completes successfully
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: toolCallId1,
      tool_name: 'ReadFiles',
      tool_result: { kind: 'ReadFiles', files: [{ path: '/mock/file.ts', bytes: 100 }] },
      success: true,
    });

    await browser.waitUntil(
      async () => {
        const statuses = await browser.execute((statusSel: string) => {
          const elements = document.querySelectorAll(statusSel);
          return Array.from(elements).slice(-2).map(el => el.textContent?.trim());
        }, sel.toolStatusText);
        return statuses[0] === 'Done';
      },
      { timeout: 3000, timeoutMsg: 'First tool should show Done' },
    );

    // Second tool starts and fails
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: toolCallId2,
      tool_name: 'ModifyFile',
      tool_type: { kind: 'ModifyFile', file_path: '/mock/broken.ts', before: 'old code', after: 'new code' },
    });

    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: toolCallId2,
      tool_name: 'ModifyFile',
      tool_result: {
        kind: 'Error',
        short_message: 'File not found: /mock/broken.ts',
        detailed_message: null,
      },
      success: false,
    });

    await browser.waitUntil(
      async () => {
        const statuses = await browser.execute((statusSel: string) => {
          const elements = document.querySelectorAll(statusSel);
          return Array.from(elements).slice(-2).map(el => el.textContent?.trim());
        }, sel.toolStatusText);
        return statuses[0] === 'Done' && statuses[1] === 'Failed';
      },
      { timeout: 3000, timeoutMsg: 'Second tool should show Failed' },
    );

    // Failed tool error message should be visible inline in the tool card
    const errorVisible = await browser.execute((toolCardSel: string) => {
      const cards = document.querySelectorAll(toolCardSel);
      const lastCard = cards[cards.length - 1];
      return lastCard?.textContent?.includes('File not found: /mock/broken.ts') ?? false;
    }, sel.toolCard);
    expect(errorVisible).toBe(true);

    // Tool cards should be inside assistant message bubbles, not orphaned
    const toolResult = await browser.execute(
      (assistantSel: string, embeddedSel: string, toolCardSel: string, chatContainerSel: string) => {
        let orphanedToolCalls = 0;
        for (const chatContainer of document.querySelectorAll(chatContainerSel)) {
          const orphaned = chatContainer.querySelector(`:scope > ${embeddedSel}`);
          if (orphaned) {
            orphanedToolCalls += orphaned.querySelectorAll(toolCardSel).length;
          }
        }
        return { orphanedToolCalls };
      },
      sel.assistantMessage,
      sel.embeddedToolCalls,
      sel.toolCard,
      sel.chatContainer,
    );
    expect(toolResult.orphanedToolCalls).toBe(0);

    // --- Mock-driven tool calls still render correctly ---
    await browser.execute(() => {
      (window as any).__mockIncludeToolCalls = true;
    });

    await sendPromptAndWaitForAssistant('tool message');
    await browser.waitUntil(
      async () => browser.execute(
        (s: string) => document.querySelectorAll(s).length >= 3,
        sel.toolCard,
      ),
      { timeout: 5000, timeoutMsg: 'Tool card did not appear for mock-driven message' },
    );

    await browser.execute(() => {
      (window as any).__mockIncludeToolCalls = false;
    });

    // --- Regression: tool output updates survive DOM disconnect/reconnect ---
    // The old bindToolOutputRenderer used isConnected to permanently remove
    // callbacks when an element was detached. This broke docked views that
    // get detached and reattached. Verify that toggling tool output mode
    // still updates a tool result after a disconnect/reconnect cycle.
    const disconnectToolId = 'test-disconnect-tool';
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Running a command for disconnect test.' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Running a command for disconnect test.', [
        { id: disconnectToolId, name: 'RunCommand', arguments: { command: 'echo hello' } },
      ]),
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: disconnectToolId,
      tool_name: 'RunCommand',
      tool_type: { kind: 'RunCommand', command: 'echo hello', working_directory: '/mock' },
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: disconnectToolId,
      tool_name: 'RunCommand',
      tool_result: { kind: 'RunCommand', exit_code: 0, stdout: 'hello\n', stderr: '' },
      success: true,
    });

    // Wait for the tool result to render with stdout content visible (compact mode is the default)
    await browser.waitUntil(
      async () => browser.execute(() => {
        const results = document.querySelectorAll('.tool-result-command');
        const last = results[results.length - 1];
        return last?.querySelector('.tool-result-stdout') !== null;
      }),
      { timeout: 5000, timeoutMsg: 'Expected RunCommand tool result to show stdout in compact mode' },
    );

    // Disconnect the tool result element from the DOM — simulates what happens
    // when a conversation view is docked (detached from the DOM tree).
    await browser.execute(() => {
      const results = document.querySelectorAll('.tool-result-command');
      const target = results[results.length - 1] as HTMLElement;
      // Stash the parent and position so we can reconnect later.
      (window as any).__disconnectTarget = target;
      (window as any).__disconnectParent = target.parentElement!;
      (window as any).__disconnectNextSibling = target.nextSibling;
      target.parentElement!.removeChild(target);
    });

    // While the element is disconnected, toggle the output mode to summary.
    // The old isConnected guard would have removed the callback during this
    // broadcast, permanently breaking updates for the element.
    await browser.execute(() => {
      const btn = document.querySelector('[data-testid="tool-output-toggle-global"]') as HTMLButtonElement | null;
      if (!btn) throw new Error('Missing tool output toggle button');
      // compact → verbose
      btn.click();
      // verbose → summary
      btn.click();
    });

    // Reconnect the element — simulates undocking the conversation view.
    await browser.execute(() => {
      const target = (window as any).__disconnectTarget as HTMLElement;
      const parent = (window as any).__disconnectParent as HTMLElement;
      const nextSibling = (window as any).__disconnectNextSibling;
      if (nextSibling) {
        parent.insertBefore(target, nextSibling);
      } else {
        parent.appendChild(target);
      }
      delete (window as any).__disconnectTarget;
      delete (window as any).__disconnectParent;
      delete (window as any).__disconnectNextSibling;
    });

    // The reconnected element should already reflect summary mode (no stdout).
    // If the callback was dropped while disconnected, the element would still
    // show the stale compact rendering with stdout visible.
    await browser.waitUntil(
      async () => browser.execute(() => {
        const results = document.querySelectorAll('.tool-result-command');
        const last = results[results.length - 1] as HTMLElement;
        return last?.querySelector('.tool-result-stdout') === null;
      }),
      { timeout: 3000, timeoutMsg: 'Tool result should reflect summary mode after disconnect/broadcast/reconnect' },
    );

    // Toggle back to compact so subsequent tests start in the expected mode
    await browser.execute(() => {
      const btn = document.querySelector('[data-testid="tool-output-toggle-global"]') as HTMLButtonElement | null;
      btn?.click(); // summary → compact
    });

    // Verify the tool result re-renders with stdout visible in compact mode
    await browser.waitUntil(
      async () => browser.execute(() => {
        const results = document.querySelectorAll('.tool-result-command');
        const last = results[results.length - 1] as HTMLElement;
        return last?.querySelector('.tool-result-stdout') !== null;
      }),
      { timeout: 3000, timeoutMsg: 'Tool result should re-render stdout after switching back to compact' },
    );

    // --- Regression: truncatable tool output in background tab preserves .collapsed ---
    // When a conversation tab is hidden (display:none), elements report scrollHeight=0
    // and clientHeight=0. Without the fix, the ResizeObserver in hideTruncationIfNotOverflowing
    // would see scrollHeight(0) <= clientHeight(0), incorrectly conclude the content
    // doesn't overflow, and remove .collapsed — breaking the "Show more" toggle.

    // Create a second conversation tab — the first tab's wrapper goes display:none
    await browser.keys(['Control', 'n']);
    await browser.waitUntil(
      async () => browser.execute((tabSel: string) => {
        return document.querySelectorAll(tabSel).length >= 2;
      }, sel.convTab),
      { timeout: 5000, timeoutMsg: 'Second conversation tab should appear' },
    );

    // Emit a tool result with long stdout to the now-hidden first tab
    const bgToolId = 'test-bg-truncation-tool';
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Running a long command.' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Running a long command.', [
        { id: bgToolId, name: 'RunCommand', arguments: { command: 'cat long.txt' } },
      ]),
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: bgToolId,
      tool_name: 'RunCommand',
      tool_type: { kind: 'RunCommand', command: 'cat long.txt', working_directory: '/mock' },
    });

    // Generate output long enough to exceed the 200px collapsed max-height
    const longOutput = Array.from(
      { length: 50 },
      (_, i) => `Line ${i + 1}: Lorem ipsum dolor sit amet, consectetur adipiscing elit.`,
    ).join('\n');
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: bgToolId,
      tool_name: 'RunCommand',
      tool_result: { kind: 'RunCommand', exit_code: 0, stdout: longOutput, stderr: '' },
      success: true,
    });

    // Give the ResizeObserver time to fire
    await browser.pause(500);

    // The truncatable element in the hidden tab should still have .collapsed.
    // Without the fix, the observer would disconnect on 0×0 dimensions and remove it.
    // The hidden conversation uses virtual scrolling, so elements are unmounted
    // when the wrapper goes display:none. Instead, test the fix by toggling tool
    // output mode while the first tab is hidden — this triggers re-rendering via
    // mode change callbacks, which call hideTruncationIfNotOverflowing on fresh
    // truncatable elements. When the tab becomes visible again and the virtualizer
    // re-mounts the elements, the ResizeObserver fires with real dimensions.

    // Toggle to verbose (no truncation) and back to compact (re-creates truncatables).
    // The compact re-render calls hideTruncationIfNotOverflowing. Since the first
    // tab's wrapper is display:none, the elements will be created inside a hidden
    // ancestor when the virtualizer re-mounts on tab switch.
    await browser.execute(() => {
      const btn = document.querySelector('[data-testid="tool-output-toggle-global"]') as HTMLButtonElement | null;
      btn?.click(); // compact → verbose
    });
    await browser.pause(100);
    await browser.execute(() => {
      const btn = document.querySelector('[data-testid="tool-output-toggle-global"]') as HTMLButtonElement | null;
      btn?.click(); // verbose → summary
    });
    await browser.pause(100);
    await browser.execute(() => {
      const btn = document.querySelector('[data-testid="tool-output-toggle-global"]') as HTMLButtonElement | null;
      btn?.click(); // summary → compact
    });
    await browser.pause(100);

    // Switch back to the first tab — virtualizer re-mounts elements and
    // ResizeObserver fires with real dimensions.
    await browser.execute((tabSel: string) => {
      const tabs = Array.from(document.querySelectorAll(tabSel)) as HTMLElement[];
      const firstTab = tabs.find((t) => !t.classList.contains('conv-tab-active'));
      firstTab?.click();
    }, sel.convTab);

    // After the tab becomes visible and the virtualizer re-mounts,
    // the long tool output should retain .truncatable.collapsed.
    // Without the fix, .collapsed would be lost because the ResizeObserver
    // fires with 0×0 dimensions in the display:none phase and incorrectly
    // concludes the content doesn't overflow.
    await browser.waitUntil(
      async () => browser.execute(() => {
        const results = document.querySelectorAll('.tool-result-command');
        const last = results[results.length - 1] as HTMLElement;
        const truncatable = last?.closest('.embedded-tool-calls')?.querySelector('.truncatable');
        return truncatable?.classList.contains('collapsed') ?? false;
      }),
      { timeout: 5000, timeoutMsg: 'Truncatable tool output should keep .collapsed after tab switch' },
    );

    // Close the second tab to restore single-tab state for subsequent tests
    await browser.execute((tabSel: string) => {
      const tabs = Array.from(document.querySelectorAll(tabSel)) as HTMLElement[];
      const secondTab = tabs.find((t) => t !== tabs[0]);
      const closeBtn = secondTab?.querySelector('.conv-tab-close') as HTMLElement | null;
      closeBtn?.click();
    }, sel.convTab);
    await browser.pause(300);

    // --- AskUserQuestion renders as question card, not a stuck "Running..." card ---
    const askToolCallId = 'test-ask-user-1';
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Let me ask you a question.' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Let me ask you a question.', [
        { id: askToolCallId, name: 'AskUserQuestion', arguments: {
          questions: [{
            question: 'Which language do you prefer?',
            header: 'Language',
            options: [
              { label: 'Rust', description: 'Systems programming' },
              { label: 'Python', description: 'Scripting language' },
            ],
            multiSelect: false,
          }],
        }},
      ]),
    });

    // AskUserQuestion should NOT create a pending tool card
    await browser.pause(100);
    const askPendingCount = await browser.execute((toolStatusSel: string) => {
      const statuses = document.querySelectorAll(toolStatusSel);
      return Array.from(statuses).filter(el => el.textContent?.trim() === 'Pending').length;
    }, sel.toolStatusText);
    expect(askPendingCount).toBe(0);

    // Emit ToolRequest — should render custom question card
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: askToolCallId,
      tool_name: 'AskUserQuestion',
      tool_type: {
        kind: 'Other',
        args: {
          tool: 'AskUserQuestion',
          arguments: {
            questions: [{
              question: 'Which language do you prefer?',
              header: 'Language',
              options: [
                { label: 'Rust', description: 'Systems programming' },
                { label: 'Python', description: 'Scripting language' },
              ],
              multiSelect: false,
            }],
          },
        },
      },
    });

    // Question text and options should be visible
    await browser.waitUntil(
      async () => browser.execute(() => {
        return document.body.textContent?.includes('Which language do you prefer?') ?? false;
      }),
      { timeout: 3000, timeoutMsg: 'AskUserQuestion text should be visible' },
    );
    const askOptions = await browser.execute(() => {
      const items = document.querySelectorAll('.ask-question-options li');
      return Array.from(items).map(li => li.textContent?.trim() ?? '');
    });
    expect(askOptions.length).toBe(2);
    expect(askOptions[0]).toContain('Rust');
    expect(askOptions[1]).toContain('Python');

    // Card status should say "Waiting for response", not "Running..."
    const askStatus = await browser.execute(() => {
      const cards = document.querySelectorAll('[data-testid="tool-card"]');
      const lastCard = cards[cards.length - 1];
      const status = lastCard?.querySelector('.tool-status-text');
      return status?.textContent?.trim() ?? '';
    });
    expect(askStatus).toBe('Waiting for response');

    // Emit ToolExecutionCompleted — card should NOT flip to "Done"
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: askToolCallId,
      tool_name: 'AskUserQuestion',
      tool_result: { kind: 'Other', result: null },
      success: true,
    });
    await browser.pause(100);
    const askStatusAfter = await browser.execute(() => {
      const cards = document.querySelectorAll('[data-testid="tool-card"]');
      const lastCard = cards[cards.length - 1];
      const status = lastCard?.querySelector('.tool-status-text');
      return status?.textContent?.trim() ?? '';
    });
    expect(askStatusAfter).toBe('Waiting for response');

    // --- ExitPlanMode renders plan content from tool result ---
    const planToolCallId = 'test-exit-plan-1';
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamDelta', { text: 'Here is my plan.' });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Here is my plan.', [
        { id: planToolCallId, name: 'ExitPlanMode', arguments: {} },
      ]),
    });

    // ExitPlanMode should NOT create a pending tool card
    await browser.pause(100);
    const planPendingCount = await browser.execute((toolStatusSel: string) => {
      const statuses = document.querySelectorAll(toolStatusSel);
      return Array.from(statuses).filter(el => el.textContent?.trim() === 'Pending').length;
    }, sel.toolStatusText);
    expect(planPendingCount).toBe(0);

    // Emit ToolRequest — initially renders as plan-mode-indicator
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: planToolCallId,
      tool_name: 'ExitPlanMode',
      tool_type: { kind: 'Other', args: { tool: 'ExitPlanMode', arguments: {} } },
    });

    await browser.waitUntil(
      async () => browser.execute(() => {
        const indicator = document.querySelector('.plan-mode-indicator');
        return indicator?.textContent?.includes('Plan ready') ?? false;
      }),
      { timeout: 3000, timeoutMsg: 'ExitPlanMode should render as "Plan ready" indicator' },
    );

    // Emit ToolExecutionCompleted with plan_content — indicator upgrades to plan card
    const planMarkdown = '## Step 1\n\nDo the first thing.\n\n## Step 2\n\nDo the second thing.';
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: planToolCallId,
      tool_name: 'ExitPlanMode',
      tool_result: { kind: 'Other', result: { plan_content: planMarkdown } },
      success: true,
    });

    // Plan content should be rendered as markdown
    await browser.waitUntil(
      async () => browser.execute(() => {
        const planContent = document.querySelector('.plan-content');
        return planContent?.textContent?.includes('Step 1') ?? false;
      }),
      { timeout: 3000, timeoutMsg: 'ExitPlanMode should render plan content as markdown' },
    );

    // Plan card should show "Plan" as name and "Ready for review" as status
    const planCardInfo = await browser.execute(() => {
      const contentEl = document.querySelector('.plan-content');
      const card = contentEl?.closest('.tool-card');
      const name = card?.querySelector('.tool-name')?.textContent?.trim();
      const status = card?.querySelector('.tool-status-text')?.textContent?.trim();
      return { name, status };
    });
    expect(planCardInfo.name).toBe('Plan');
    expect(planCardInfo.status).toBe('Ready for review');

    // No stuck statuses
    const planHasStuckStatus = await browser.execute(() => {
      const contentEl = document.querySelector('.plan-content');
      const container = contentEl?.closest('.embedded-tool-calls');
      if (!container) return false;
      const statusTexts = Array.from(container.querySelectorAll('.tool-status-text'))
        .map(el => el.textContent?.trim());
      return statusTexts.includes('Running...') || statusTexts.includes('Pending');
    });
    expect(planHasStuckStatus).toBe(false);

    // --- Sessions panel loads data ---
    await openSessionsWidget();
    await browser.waitUntil(
      async () => (await getVisibleSessionsPanelText()).length > 0,
      { timeout: 5000, timeoutMsg: 'Expected visible sessions panel in workspace view' },
    );
    await clickVisibleSessionsRefresh('Visible sessions refresh button was not clickable');

    await browser.waitUntil(
      async () => {
        const text = await getVisibleSessionsPanelText();
        return text.includes('Tycode Session 1')
          && text.includes('Codex Session 1')
          && !text.includes('No active project');
      },
      { timeout: 5000, timeoutMsg: 'Sessions panel should display merged backend session data' },
    );

    await clickVisibleSessionsRefresh('Visible sessions refresh button was not clickable on second click');
    await browser.waitUntil(
      async () => {
        const text = await getVisibleSessionsPanelText();
        return text.includes('Tycode Session 1') && text.includes('Codex Session 1');
      },
      { timeout: 5000, timeoutMsg: 'Sessions should reload after second refresh' },
    );

    // --- SubprocessExit shows crash notice and disables send (LAST) ---
    await emitChatEvent(WORKSPACE_CONV_ID, 'SubprocessExit', { exit_code: 1 });

    await browser.waitUntil(
      async () => {
        const errorMsgs = await $$(sel.errorMessage);
        for (const msg of errorMsgs) {
          const text = await msg.getText();
          if (text.includes('Backend process exited')) return true;
        }
        return false;
      },
      { timeout: 5000, timeoutMsg: 'Backend process exited notice should appear' },
    );

    await browser.waitUntil(
      async () => {
        const disabled = await sendBtn.getAttribute('disabled');
        return disabled !== null;
      },
      { timeout: 5000, timeoutMsg: 'Send button should be disabled after SubprocessExit' },
    );
  });

  it('clears stale context usage when the latest assistant message has no breakdown', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    await sendPromptAndWaitForAssistant('Hello');

    await browser.waitUntil(
      async () => {
        const usage = await $(sel.contextUsage);
        return (await usage.getText()).includes('50.0K');
      },
      { timeout: 5000, timeoutMsg: 'Expected initial context usage to be visible' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Understood.'),
    });

    await browser.waitUntil(
      async () => (await (await $$(sel.contextUsage)).length) === 0,
      { timeout: 5000, timeoutMsg: 'Context usage should clear when breakdown is missing' },
    );

    const bar = await $(sel.contextBar);
    await bar.waitForExist({ timeout: 5000 });
    expect(await bar.getAttribute('aria-valuenow')).toBe('0');

    const segments = await bar.$$(sel.contextSegment);
    expect(segments.length).toBe(0);
  });
});

describe('Sessions backend behavior', () => {
  it('loads merged backend sessions, resumes into a new tab, and deletes only the selected backend session', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await openSessionsWidget();
    await browser.waitUntil(
      async () => (await getVisibleSessionsPanelText()).length > 0,
      { timeout: 5000, timeoutMsg: 'Expected visible sessions panel in workspace view' },
    );
    await clickVisibleSessionsRefresh('Visible sessions refresh button was not clickable');

    await browser.waitUntil(
      async () => {
        const text = await getVisibleSessionsPanelText();
        return text.includes('Tycode Session 1') && text.includes('Codex Session 1');
      },
      { timeout: 5000, timeoutMsg: 'Expected merged Tycode and Codex sessions' },
    );

    const initialChatTabCount = await browser.execute(() => {
      return document.querySelectorAll('[data-testid="conv-tab"]:not(.conv-tab-file)').length;
    });
    expect(initialChatTabCount).toBeGreaterThan(0);

    await browser.execute(() => {
      const cards = Array.from(document.querySelectorAll('[data-testid="session-card"]')) as HTMLElement[];
      const codexCard = cards.find((card) => (card.textContent ?? '').includes('Codex Session 1'));
      if (!codexCard) throw new Error('Missing Codex session card');
      codexCard.click();
    });

    await browser.waitUntil(
      async () => {
        const count = await browser.execute(() => {
          return document.querySelectorAll('[data-testid="conv-tab"]:not(.conv-tab-file)').length;
        });
        return Number(count) === Number(initialChatTabCount) + 1;
      },
      { timeout: 5000, timeoutMsg: 'Resuming a session should always open a new chat tab' },
    );

    await clickVisibleSessionsRefresh('Visible sessions refresh button was not clickable before delete check');
    await browser.waitUntil(
      async () => {
        const text = await getVisibleSessionsPanelText();
        return text.includes('Tycode Session 1') && text.includes('Codex Session 1');
      },
      { timeout: 5000, timeoutMsg: 'Expected both backend sessions before delete' },
    );

    // --- Rename the resumed Codex tab via double-click ---
    await browser.execute((tabTitleSel: string) => {
      const activeTab = document.querySelector('[data-testid="conv-tab"].conv-tab-active');
      const titleEl = activeTab?.querySelector(tabTitleSel) as HTMLElement | null;
      titleEl?.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    }, '[data-testid="conv-tab-title"]');

    await browser.waitUntil(
      async () => (await (await $$('.conv-tab-rename-input')).length) > 0,
      { timeout: 5000, timeoutMsg: 'Expected rename input to appear' },
    );

    await browser.execute((newName: string) => {
      const renameInput = document.querySelector('.conv-tab-rename-input') as HTMLInputElement | null;
      if (!renameInput) return;
      renameInput.value = newName;
      renameInput.dispatchEvent(new Event('input', { bubbles: true }));
      renameInput.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    }, 'My Codex Chat');

    await browser.waitUntil(
      async () => {
        const title = await (await $(sel.convTabActive + ' ' + sel.convTabTitle)).getText();
        return title === 'My Codex Chat';
      },
      { timeout: 5000, timeoutMsg: 'Expected tab rename to apply' },
    );

    // --- Sessions panel should show the alias after refresh ---
    await clickVisibleSessionsRefresh('Visible sessions refresh button was not clickable after rename');
    await browser.waitUntil(
      async () => {
        const text = await getVisibleSessionsPanelText();
        return text.includes('My Codex Chat');
      },
      { timeout: 5000, timeoutMsg: 'Sessions panel should show the renamed alias' },
    );

    // --- Search in sessions panel should find by alias ---
    await setVisibleSessionsSearch('My Codex');

    await browser.waitUntil(
      async () => {
        const cardTexts = await getVisibleSessionCardTexts();
        return cardTexts.length > 0 && cardTexts.some((text) => text.includes('My Codex Chat'));
      },
      { timeout: 3000, timeoutMsg: 'Search by alias should find the session' },
    );

    // Tycode session should be filtered out by this search
    await browser.waitUntil(
      async () => {
        const cardTexts = await getVisibleSessionCardTexts();
        return cardTexts.length > 0 && cardTexts.every((text) => !text.includes('Tycode Session 1'));
      },
      { timeout: 3000, timeoutMsg: 'Search should filter out non-matching sessions' },
    );

    // Clear search
    await setVisibleSessionsSearch('');

    // --- Delete the Codex session: should close its tab ---
    const tabCountBeforeDelete = await browser.execute(() =>
      document.querySelectorAll('[data-testid="conv-tab"]:not(.conv-tab-file)').length,
    );

    await browser.execute(() => {
      (window as any).__mockDialogConfirm = true;
    });

    await clickVisibleSessionsRefresh('Visible sessions refresh button was not clickable before delete');
    await browser.waitUntil(
      async () => (await getVisibleSessionsPanelText()).includes('My Codex Chat'),
      { timeout: 5000, timeoutMsg: 'Expected Codex session before delete' },
    );

    await browser.execute(() => {
      const cards = Array.from(document.querySelectorAll('[data-testid="session-card"]')) as HTMLElement[];
      const codexCard = cards.find((card) => (card.textContent ?? '').includes('My Codex Chat'));
      if (!codexCard) throw new Error('Missing Codex session card before delete');
      const deleteBtn = codexCard.querySelector('button[title="Delete session"]') as HTMLButtonElement | null;
      if (!deleteBtn) throw new Error('Missing delete button on Codex session card');
      deleteBtn.click();
    });

    // Tab count should decrease — deleting closes the associated tab
    await browser.waitUntil(
      async () => {
        const count = await browser.execute(() =>
          document.querySelectorAll('[data-testid="conv-tab"]:not(.conv-tab-file)').length,
        );
        return Number(count) < Number(tabCountBeforeDelete);
      },
      { timeout: 5000, timeoutMsg: 'Deleting session should close its tab' },
    );

    // Codex session gone, Tycode session remains
    await browser.waitUntil(
      async () => {
        const text = await getVisibleSessionsPanelText();
        return !text.includes('Codex Session 1') && !text.includes('My Codex Chat') && text.includes('Tycode Session 1');
      },
      { timeout: 5000, timeoutMsg: 'Deleting Codex session should not remove Tycode sessions' },
    );

    await browser.execute(() => {
      delete (window as any).__mockDialogConfirm;
    });
  });
});

describe('Claude resume parity', () => {
  it('renders replayed Claude tool cards and image attachments from restored history', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    const toolCallId = 'claude-resume-tool-1';

    await emitChatEvent(WORKSPACE_CONV_ID, 'ConversationCleared', {});
    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      timestamp: Date.now(),
      sender: { Assistant: { agent: 'claude' } },
      content: 'Replaying a Claude tool call from session history.',
      reasoning: null,
      tool_calls: [
        {
          id: toolCallId,
          name: 'Bash',
          arguments: { command: 'ls -la' },
        },
      ],
      model_info: { model: 'claude-opus-4-6' },
      token_usage: null,
      context_breakdown: null,
      images: [],
    });

    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: toolCallId,
      tool_name: 'Bash',
      tool_type: {
        kind: 'RunCommand',
        command: 'ls -la',
        working_directory: '/mock/workspace',
      },
    });

    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: toolCallId,
      tool_name: 'Bash',
      tool_result: { kind: 'Other', result: 'done' },
      success: true,
    });

    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      timestamp: Date.now(),
      sender: 'User',
      content: 'Screenshot attached',
      reasoning: null,
      tool_calls: [],
      model_info: null,
      token_usage: null,
      context_breakdown: null,
      images: [
        {
          media_type: 'image/png',
          data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO7m6i0AAAAASUVORK5CYII=',
        },
      ],
    });

    await browser.waitUntil(
      async () => browser.execute((statusSel: string) => {
        const statuses = Array.from(document.querySelectorAll(statusSel))
          .map((el) => el.textContent?.trim() ?? '');
        return statuses.includes('Done');
      }, sel.toolStatusText),
      { timeout: 5000, timeoutMsg: 'Expected resumed Claude tool card to reach Done state' },
    );

    const hasUserImage = await browser.execute((chatMessageSel: string) => {
      const messages = Array.from(document.querySelectorAll(chatMessageSel)) as HTMLElement[];
      return messages.some((message) => {
        if (!message.classList.contains('user-message')) return false;
        return !!message.querySelector('img[src^="data:image/png;base64,"]');
      });
    }, sel.chatMessage);
    expect(hasUserImage).toBe(true);

    const orphanedToolCards = await browser.execute((chatContainerSel: string, embeddedSel: string, toolCardSel: string) => {
      let orphaned = 0;
      for (const chatContainer of document.querySelectorAll(chatContainerSel)) {
        const directToolContainer = chatContainer.querySelector(`:scope > ${embeddedSel}`);
        if (directToolContainer) {
          orphaned += directToolContainer.querySelectorAll(toolCardSel).length;
        }
      }
      return orphaned;
    }, sel.chatContainer, sel.embeddedToolCalls, sel.toolCard);
    expect(orphanedToolCards).toBe(0);
  });

  it('anchors replayed Claude diffs to the original assistant message and opens the diff viewer', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    const toolCallId = 'claude-replay-edit-1';

    await emitChatEvent(WORKSPACE_CONV_ID, 'ConversationCleared', {});
    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      timestamp: Date.now(),
      sender: { Assistant: { agent: 'claude' } },
      content: 'First Claude API call',
      reasoning: null,
      tool_calls: [
        {
          id: toolCallId,
          name: 'Edit',
          arguments: { file_path: '/mock/app.ts' },
        },
      ],
      model_info: { model: 'claude-opus-4-6' },
      token_usage: null,
      context_breakdown: null,
      images: [],
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', {
      timestamp: Date.now(),
      sender: { Assistant: { agent: 'claude' } },
      content: 'Second Claude API call',
      reasoning: null,
      tool_calls: [],
      model_info: { model: 'claude-opus-4-6' },
      token_usage: null,
      context_breakdown: null,
      images: [],
    });

    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolRequest', {
      tool_call_id: toolCallId,
      tool_name: 'Edit',
      tool_type: {
        kind: 'ModifyFile',
        file_path: '/mock/app.ts',
        before: 'const value = 1;\n',
        after: 'const value = 2;\n',
      },
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'ToolExecutionCompleted', {
      tool_call_id: toolCallId,
      tool_name: 'Edit',
      tool_result: {
        kind: 'ModifyFile',
        lines_added: 1,
        lines_removed: 1,
      },
      success: true,
    });

    await browser.waitUntil(
      async () => browser.execute(() => {
        return Array.from(document.querySelectorAll('.assistant-message'))
          .some((message) => !!message.querySelector('.view-diff-btn'));
      }),
      { timeout: 5000, timeoutMsg: 'Expected replayed Claude diff button to appear' },
    );

    const messagePlacement = await browser.execute(() => {
      const messages = Array.from(document.querySelectorAll('.assistant-message')) as HTMLElement[];
      return messages.slice(-2).map((message) => ({
        text: (message.querySelector('.message-content')?.textContent ?? '').trim(),
        hasToolCard: !!message.querySelector('[data-testid="tool-card"]'),
        hasDiffBtn: !!message.querySelector('.view-diff-btn'),
      }));
    });
    expect(messagePlacement[0]).toEqual({
      text: 'First Claude API call',
      hasToolCard: true,
      hasDiffBtn: true,
    });
    expect(messagePlacement[1]).toEqual({
      text: 'Second Claude API call',
      hasToolCard: false,
      hasDiffBtn: false,
    });

    await browser.execute(() => {
      const firstDiffBtn = document.querySelector('.assistant-message .view-diff-btn') as HTMLButtonElement | null;
      firstDiffBtn?.click();
    });

    await browser.waitUntil(
      async () => browser.execute(() => {
        const active = document.querySelector('[data-testid="conv-tab"].conv-tab-active');
        const title = active?.querySelector('[data-testid="conv-tab-title"]')?.textContent?.trim() ?? '';
        return title === 'app.ts';
      }),
      { timeout: 5000, timeoutMsg: 'Expected diff tab to open for replayed Claude edit' },
    );

    await browser.waitUntil(
      async () => browser.execute(() => {
        const removed = Array.from(document.querySelectorAll('.diff-panel-removed .diff-panel-line-text'))
          .map((node) => node.textContent ?? '');
        const added = Array.from(document.querySelectorAll('.diff-panel-added .diff-panel-line-text'))
          .map((node) => node.textContent ?? '');
        return removed.some((line) => line.includes('const value = 1;'))
          && added.some((line) => line.includes('const value = 2;'));
      }),
      { timeout: 5000, timeoutMsg: 'Expected replayed Claude diff contents to render in diff viewer' },
    );
  });
});

describe('Agents panel parity', () => {
  it('shows normal chat conversations in the agents panel', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Working before typing false'),
    });

    await openAgentsWidget();

    await browser.waitUntil(
      async () => (await (await $$(sel.agentCard)).length) >= 1,
      { timeout: 5000, timeoutMsg: 'Expected at least one agent card after creating a chat' },
    );
    await browser.waitUntil(
      async () => (await (await $$(sel.agentCardRunning)).length) >= 1,
      { timeout: 5000, timeoutMsg: 'Expected agent to remain running until TypingStatusChanged=false' },
    );
    await browser.waitUntil(
      async () => browser.execute((runningSel: string, interruptSel: string) => {
        const runningCards = Array.from(document.querySelectorAll(runningSel)) as HTMLElement[];
        return runningCards.some((card) => {
          if (!card.offsetParent) return false;
          return Boolean(card.querySelector(interruptSel));
        });
      }, sel.agentCardRunning, sel.agentCardInterrupt),
      { timeout: 5000, timeoutMsg: 'Expected Interrupt action on running conversation card' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);
    await browser.waitUntil(
      async () => (await (await $$(sel.agentCardRunning)).length) === 0,
      { timeout: 5000, timeoutMsg: 'Expected no running cards after TypingStatusChanged=false' },
    );
    await browser.waitUntil(
      async () => browser.execute((cardSel: string, removeSel: string) => {
        const cards = Array.from(document.querySelectorAll(cardSel)) as HTMLElement[];
        return cards.some((card) => {
          if (!card.offsetParent) return false;
          return Boolean(card.querySelector(removeSel));
        });
      }, sel.agentCard, sel.agentCardRemove),
      { timeout: 5000, timeoutMsg: 'Expected Remove action on completed conversation card' },
    );

    const titles = await browser.execute((titleSel: string) => {
      return Array.from(document.querySelectorAll(titleSel))
        .map((el) => el.textContent?.trim() ?? '')
        .filter(Boolean);
    }, sel.agentCardTitle);

    expect(titles.some((t) => t.startsWith('Chat'))).toBe(true);
  });

  it('retains feedback-agent conversation history when opened after background execution', async () => {
    await openWorkspace();

    // Set up mock to return updated content when the file is re-read after feedback.
    await browser.execute(() => {
      (window as any).__mockReadFileContentByPath = {
        '/mock/workspace/README.md': '# Updated heading\n',
      };
    });

    const feedbackId = await spawnFeedbackAgent(
      '/mock/workspace/README.md',
      '# Old content',
      'Update the heading to be clearer',
    );
    expect(feedbackId).toBeGreaterThan(0);
    await openAgentsWidget();

    await browser.waitUntil(
      async () => (await (await $$(sel.agentCard)).length) >= 1,
      { timeout: 5000, timeoutMsg: 'Expected feedback agent card to appear' },
    );
    await browser.waitUntil(
      async () => (await (await $$(sel.agentCardRunning)).length) === 0,
      { timeout: 5000, timeoutMsg: 'Expected no running agent cards after typing has stopped' },
    );

    await browser.execute((cardSel: string) => {
      const card = Array.from(document.querySelectorAll(cardSel))
        .find((el) => (el as HTMLElement).offsetParent !== null) as HTMLElement | undefined;
      card?.click();
    }, sel.agentCard);

    await browser.waitUntil(
      async () => {
        const msgs = await $$(sel.assistantMessage);
        for (const msg of msgs) {
          const text = await msg.getText();
          if (text.includes('Mock response to:')) return true;
        }
        return false;
      },
      { timeout: 5000, timeoutMsg: 'Expected assistant history with mock response text when opening feedback agent' },
    );

    const assistantMessages = await $$(sel.assistantMessage);
    const msgCount = await assistantMessages.length;
    const latestText = await assistantMessages[msgCount - 1].getText();
    expect(latestText).toContain('Mock response to: Apply the following feedback to file: /mock/workspace/README.md');

    // --- Regression: feedback agents complete via TypingStatusChanged ---
    // The mock backend emits TypingStatusChanged(false) but never SubprocessExit.
    // The feedback agent must still be marked completed (not running) in
    // the agents panel. Before the fix, the feedback handler only listened
    // for SubprocessExit, leaving the inline spinner stuck in progress.
    await openAgentsWidget();
    expect((await $$(sel.agentCardRunning)).length).toBe(0);
    const cardIsCompleted = await browser.execute((cardSel: string) => {
      const cards = Array.from(document.querySelectorAll(cardSel)) as HTMLElement[];
      const visible = cards.find((el) => el.offsetParent !== null);
      return visible?.classList.contains('agent-card-completed') ?? false;
    }, sel.agentCard);
    expect(cardIsCompleted).toBe(true);

    // Switch back to the file tab so the diff panel is visible for assertions.
    await browser.execute(() => {
      const fileTab = document.querySelector('[data-testid="conv-tab"].conv-tab-file') as HTMLElement | null;
      fileTab?.click();
    });

    // --- Bug A: feedback box in diff panel shows "complete" (not stuck on spinner) ---
    // The inline feedback box should transition from a spinner to a checkmark icon
    // when the feedback agent finishes via TypingStatusChanged(false).
    await browser.waitUntil(
      async () => browser.execute(() => document.querySelector('.feedback-complete-icon') !== null),
      { timeout: 5000, timeoutMsg: 'Expected feedback box to show completion icon' },
    );
    const feedbackBoxStatus = await browser.execute(() => {
      const completeIcon = document.querySelector('.feedback-complete-icon');
      const spinner = document.querySelector('.feedback-spinner');
      return {
        hasCompleteIcon: completeIcon !== null,
        hasSpinner: spinner !== null,
        completeIconText: completeIcon?.textContent ?? null,
      };
    });
    expect(feedbackBoxStatus.hasCompleteIcon).toBe(true);
    expect(feedbackBoxStatus.hasSpinner).toBe(false);
    expect(feedbackBoxStatus.completeIconText).toBe('✓');

    // --- Bug B: open file tab content refreshed after feedback agent modifies it ---
    // The file was opened with '# Old content' but the mock read_file_content now
    // returns '# Updated heading\n'. The refresh triggered on TypingStatusChanged(false)
    // should have updated the tab content.
    await browser.waitUntil(
      async () => browser.execute(() => {
        const lines = document.querySelectorAll('.diff-panel-file-line');
        return Array.from(lines).some((el) => (el.textContent ?? '').includes('Updated heading'));
      }),
      { timeout: 5000, timeoutMsg: 'Expected file tab content to be refreshed with updated content' },
    );
  });

  it('shows runtime agents in both the project and home agents views and supports interrupt/remove controls', async () => {
    await openWorkspace();

    await spawnRuntimeAgent('Bridge Worker', { completionDelayMs: 15000 });
    await openAgentsWidget();

    await browser.waitUntil(
      async () => {
        return browser.execute((titleSel: string) => {
          return Array.from(document.querySelectorAll(titleSel))
            .filter((el) => (el as HTMLElement).offsetParent !== null)
            .map((el) => el.textContent?.trim() ?? '')
            .includes('Bridge Worker');
        }, sel.agentCardTitle);
      },
      { timeout: 5000, timeoutMsg: 'Expected runtime agent to appear in the project Agents widget' },
    );

    await browser.waitUntil(
      async () => browser.execute((intSel: string, termSel: string) => {
        const visible = (s: string) => Array.from(document.querySelectorAll(s))
          .find((el) => (el as HTMLElement).offsetParent !== null) as HTMLElement | undefined;
        return !!(visible(intSel) && visible(termSel));
      }, sel.agentCardInterrupt, sel.agentCardTerminate),
      { timeout: 5000, timeoutMsg: 'Expected interrupt and terminate buttons to be visible on the runtime agent card' },
    );
    await browser.execute((intSel: string) => {
      const btn = Array.from(document.querySelectorAll(intSel))
        .find((el) => (el as HTMLElement).offsetParent !== null) as HTMLElement | undefined;
      btn?.click();
    }, sel.agentCardInterrupt);

    await browser.waitUntil(
      async () => (await (await $$(sel.agentCardRemove)).length) > 0,
      { timeout: 5000, timeoutMsg: 'Expected Remove button after interrupting the runtime agent' },
    );

    const homeRail = await $(sel.railHomeItem);
    await homeRail.waitForClickable({ timeout: 5000 });
    await homeRail.click();

    const homeAgentsTab = await $(sel.homeTabAgents);
    await homeAgentsTab.waitForClickable({ timeout: 5000 });
    await homeAgentsTab.click();

    await browser.waitUntil(
      async () => {
        return browser.execute((cardSel: string) => {
          return Array.from(document.querySelectorAll(cardSel))
            .filter((el) => (el as HTMLElement).offsetParent !== null)
            .map((el) => el.textContent ?? '')
            .some((text) => text.includes('Bridge Worker'));
        }, sel.homeAgentCard);
      },
      { timeout: 5000, timeoutMsg: 'Expected runtime agent to appear in the home Agents tab' },
    );

    await browser.execute((cardSel: string, buttonSel: string) => {
      const card = Array.from(document.querySelectorAll(cardSel))
        .find((el) => (el as HTMLElement).offsetParent !== null && (el.textContent ?? '').includes('Bridge Worker'));
      const btn = card?.querySelector(buttonSel) as HTMLButtonElement | null;
      btn?.click();
    }, sel.homeAgentCard, sel.agentCardRemove);

    await browser.waitUntil(
      async () => {
        return browser.execute((cardSel: string) => {
          return !Array.from(document.querySelectorAll(cardSel))
            .filter((el) => (el as HTMLElement).offsetParent !== null)
            .map((el) => el.textContent ?? '')
            .some((text) => text.includes('Bridge Worker'));
        }, sel.homeAgentCard);
      },
      { timeout: 5000, timeoutMsg: 'Expected removed runtime agent to disappear from the home Agents tab' },
    );

    const workspaceRail = await $(sel.railProjectItem);
    await workspaceRail.waitForClickable({ timeout: 5000 });
    await workspaceRail.click();
    await openAgentsWidget();

    await browser.waitUntil(
      async () => {
        return browser.execute((titleSel: string) => {
          return !Array.from(document.querySelectorAll(titleSel))
            .filter((el) => (el as HTMLElement).offsetParent !== null)
            .map((el) => el.textContent?.trim() ?? '')
            .includes('Bridge Worker');
        }, sel.agentCardTitle);
      },
      { timeout: 5000, timeoutMsg: 'Expected removed runtime agent to disappear from the project Agents widget' },
    );
  });

  it('marks a bridge sub-agent as completed when TypingStatusChanged false is received', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);
    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await openAgentsWidget();

    // Register a sub-agent with a real agent_id so it participates in the
    // list_agents → syncRuntimeAgents polling path (the actual bug path).
    const subAgentConvId = WORKSPACE_CONV_ID + 500;
    const subAgentId = 42;
    await emitChatEvent(subAgentConvId, 'ConversationRegistered', {
      agent_id: subAgentId,
      workspace_roots: ['/mock/workspace'],
      backend_kind: 'tycode',
      name: 'Sub-agent Worker',
      agent_type: null,
      parent_agent_id: 1,
    });

    // Also register the agent in the mock runtime so list_agents returns it
    // with is_running: true (simulating a live runtime-tracked sub-agent).
    const now = Date.now();
    await browser.execute((agent: any) => {
      (window as any).__mockSetRuntimeAgent(agent);
    }, {
      agent_id: subAgentId,
      conversation_id: subAgentConvId,
      workspace_roots: ['/mock/workspace'],
      backend_kind: 'tycode',
      parent_agent_id: 1,
      keep_alive_without_tab: false,
      name: 'Sub-agent Worker',
      is_running: true,
      summary: '',
      created_at_ms: now,
      updated_at_ms: now,
      ended_at_ms: null,
      last_error: null,
      last_message: null,
    });

    await emitChatEvent(subAgentConvId, 'TypingStatusChanged', true);

    // Sub-agent card should appear as running
    await browser.waitUntil(
      async () => browser.execute((titleSel: string) => {
        return Array.from(document.querySelectorAll(titleSel))
          .filter((el) => (el as HTMLElement).offsetParent !== null)
          .map((el) => el.textContent?.trim() ?? '')
          .includes('Sub-agent Worker');
      }, sel.agentCardTitle),
      { timeout: 5000, timeoutMsg: 'Expected sub-agent card to appear in agents panel' },
    );
    await browser.waitUntil(
      async () => (await (await $$(sel.agentCardRunning)).length) >= 1,
      { timeout: 5000, timeoutMsg: 'Expected sub-agent card to show running state' },
    );

    // Sub-agent completes: TypingStatusChanged false
    await emitChatEvent(subAgentConvId, 'TypingStatusChanged', false);

    // Simulate the Rust fix: record_chat_event sets is_running to false in the
    // runtime, so the next list_agents poll returns the agent as stopped.
    await browser.execute((agentId: number, convId: number) => {
      (window as any).__mockSetRuntimeAgent({
        agent_id: agentId,
        conversation_id: convId,
        workspace_roots: ['/mock/workspace'],
        backend_kind: 'tycode',
        parent_agent_id: 1,
        keep_alive_without_tab: false,
        name: 'Sub-agent Worker',
        is_running: false,
        summary: 'Completed',
        created_at_ms: Date.now(),
        updated_at_ms: Date.now(),
        ended_at_ms: Date.now(),
        last_error: null,
        last_message: null,
      });
    }, subAgentId, subAgentConvId);

    // Wait for at least one syncRuntimeAgents poll cycle (runs every 500ms)
    // to verify the poll doesn't revert the completed state.
    await browser.pause(800);

    // The sub-agent card should still show completed state (no running class,
    // shows remove button instead of interrupt/terminate).
    await browser.waitUntil(
      async () => browser.execute((cardSel: string, titleSel: string) => {
        const cards = Array.from(document.querySelectorAll(cardSel)) as HTMLElement[];
        return !cards.some((card) => {
          if (!card.offsetParent) return false;
          const title = card.querySelector(titleSel);
          return title?.textContent?.trim() === 'Sub-agent Worker'
            && card.classList.contains('agent-card-running');
        });
      }, sel.agentCard, sel.agentCardTitle),
      { timeout: 5000, timeoutMsg: 'Expected sub-agent card to stay completed after syncRuntimeAgents poll' },
    );
    await browser.waitUntil(
      async () => browser.execute((cardSel: string, removeSel: string, titleSel: string) => {
        const cards = Array.from(document.querySelectorAll(cardSel)) as HTMLElement[];
        return cards.some((card) => {
          if (!card.offsetParent) return false;
          const title = card.querySelector(titleSel);
          if (!title || title.textContent?.trim() !== 'Sub-agent Worker') return false;
          return Boolean(card.querySelector(removeSel));
        });
      }, sel.agentCard, sel.agentCardRemove, sel.agentCardTitle),
      { timeout: 5000, timeoutMsg: 'Expected Remove button on completed sub-agent card after poll' },
    );
  });

  it('reopens hidden agent chats from the agents widget without duplicating tabs or losing live updates', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamStart', { agent: 'tycode', model: null });
    await emitChatEvent(WORKSPACE_CONV_ID, 'StreamEnd', {
      message: makeAssistantMessage('Initial visible response'),
    });
    await emitChatEvent(WORKSPACE_CONV_ID, 'TypingStatusChanged', false);

    await browser.execute(() => {
      const closeBtn = document.querySelector('.conv-tab-close') as HTMLButtonElement | null;
      closeBtn?.click();
    });

    await browser.waitUntil(
      async () => (await (await $$(sel.convTab)).length) === 0,
      { timeout: 5000, timeoutMsg: 'Expected the chat tab to close' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'MessageAdded', makeAssistantMessage('Background update while hidden'));

    await openAgentsWidget();

    await browser.waitUntil(
      async () => (await (await $$(sel.agentCard)).length) >= 1,
      { timeout: 5000, timeoutMsg: 'Expected the hidden conversation to remain in the agents panel' },
    );

    await browser.execute((cardSel: string) => {
      const card = Array.from(document.querySelectorAll(cardSel))
        .find((el) => (el as HTMLElement).offsetParent !== null) as HTMLElement | undefined;
      card?.click();
    }, sel.agentCard);

    await browser.waitUntil(
      async () => (await (await $$(sel.convTab)).length) === 1,
      { timeout: 5000, timeoutMsg: 'Expected clicking the agent card to unhide a single chat tab' },
    );

    await browser.waitUntil(
      async () => {
        return browser.execute((messageSel: string) => {
          return Array.from(document.querySelectorAll(messageSel))
            .filter((el) => (el as HTMLElement).offsetParent !== null)
            .map((el) => el.textContent ?? '')
            .some((text) => text.includes('Background update while hidden'));
        }, sel.assistantMessage);
      },
      { timeout: 5000, timeoutMsg: 'Expected hidden chat DOM to keep updating in the background' },
    );

    await openAgentsWidget();
    await browser.execute((cardSel: string) => {
      const card = Array.from(document.querySelectorAll(cardSel))
        .find((el) => (el as HTMLElement).offsetParent !== null) as HTMLElement | undefined;
      card?.click();
    }, sel.agentCard);

    const visibleInput = await $(sel.messageInput);
    await visibleInput.waitForDisplayed({ timeout: 5000 });
    expect((await $$(sel.convTab)).length).toBe(1);
  });
});

describe('Chat tab auto title', () => {
  it('auto-renames from task updates unless the user has manually renamed the tab', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    await browser.waitUntil(
      async () => {
        const title = await $(sel.convTabTitle);
        return (await title.getText()).startsWith('Chat');
      },
      { timeout: 5000, timeoutMsg: 'Expected initial chat tab title' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'TaskUpdate', {
      title: 'Implement user auth flow',
      tasks: [
        { id: 1, description: 'Plan auth workflow', status: 'in_progress' },
      ],
    });

    await browser.waitUntil(
      async () => {
        const title = await $(sel.convTabTitle);
        return (await title.getText()) === 'Implement user auth';
      },
      { timeout: 5000, timeoutMsg: 'Expected tab title to auto-update from task title' },
    );

    await browser.execute((tabTitleSel: string) => {
      const titleEl = document.querySelector(tabTitleSel) as HTMLElement | null;
      titleEl?.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    }, sel.convTabTitle);

    await browser.waitUntil(
      async () => (await (await $$('.conv-tab-rename-input')).length) > 0,
      { timeout: 5000, timeoutMsg: 'Expected rename input to appear' },
    );

    await browser.execute((newName: string) => {
      const input = document.querySelector('.conv-tab-rename-input') as HTMLInputElement | null;
      if (!input) return;
      input.value = newName;
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    }, 'Manual Name');

    await browser.waitUntil(
      async () => {
        const title = await $(sel.convTabTitle);
        return (await title.getText()) === 'Manual Name';
      },
      { timeout: 5000, timeoutMsg: 'Expected manual tab rename to apply' },
    );

    await emitChatEvent(WORKSPACE_CONV_ID, 'TaskUpdate', {
      title: 'Refactor cache invalidation',
      tasks: [
        { id: 2, description: 'Update cache paths', status: 'in_progress' },
      ],
    });

    await browser.pause(250);
    const finalTitle = await (await $(sel.convTabTitle)).getText();
    expect(finalTitle).toBe('Manual Name');
  });

  it('renames the tab via title agent after the first user message is sent', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    // Tab starts with a default "Chat N" title
    await browser.waitUntil(
      async () => {
        const title = await $(sel.convTabActive + ' ' + sel.convTabTitle);
        return (await title.getText()).startsWith('Chat');
      },
      { timeout: 5000, timeoutMsg: 'Expected initial default chat tab title' },
    );

    // Sending the first message triggers auto-title generation via a spawned title agent
    await sendPromptAndWaitForAssistant('Fix the login page redirect bug');

    // The title agent completes asynchronously and renames the tab
    await browser.waitUntil(
      async () => {
        const title = await $(sel.convTabActive + ' ' + sel.convTabTitle);
        const text = await title.getText();
        return !text.startsWith('Chat');
      },
      { timeout: 10000, timeoutMsg: 'Expected tab title to change from default after title agent completes' },
    );

    const autoTitle = await (await $(sel.convTabActive + ' ' + sel.convTabTitle)).getText();
    expect(autoTitle.length).toBeGreaterThan(0);

    // Verify the agents panel card also received the auto-generated title
    await openAgentsWidget();
    await browser.waitUntil(
      async () => {
        return browser.execute((titleSel: string, expected: string) => {
          return Array.from(document.querySelectorAll(titleSel))
            .filter((el) => (el as HTMLElement).offsetParent !== null)
            .some((el) => el.textContent?.trim() === expected);
        }, sel.agentCardTitle, autoTitle);
      },
      { timeout: 5000, timeoutMsg: 'Expected agents panel card title to match the auto-generated tab title' },
    );
  });
});

describe('Chat file links', () => {
  it('opens markdown file links in the file viewer with and without line numbers', async () => {
    await openWorkspace();

    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);

    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    const repoRoot = process.cwd().replace(/\\/g, '/');
    const filePathWithLine = `${repoRoot}/src-tauri/src/agent_mcp_http.rs`;
    const fileHrefWithLine = `${filePathWithLine}:250`;
    const fileHrefWithoutLine = `${repoRoot}/src/settings.ts`;

    await browser.execute((linePath: string, plainPath: string) => {
      const longMock = Array.from({ length: 400 }, (_, i) => `line ${i + 1}`).join('\n');
      (window as any).__mockReadFileContentByPath = {
        [linePath]: longMock,
        [plainPath]: 'settings value from mock\n',
      };
    }, filePathWithLine, fileHrefWithoutLine);

    const content = [
      `Jump here: [agent_mcp_http.rs:250](${fileHrefWithLine})`,
      `Also open this: [settings.ts](${fileHrefWithoutLine})`,
    ].join('\n');
    await sendPromptAndWaitForAssistant(content);

    const clickAssistantLink = async (label: string): Promise<void> => {
      try {
        await browser.waitUntil(
          async () => browser.execute((targetLabel: string) => {
            const links = Array.from(document.querySelectorAll('.assistant-message a')) as HTMLAnchorElement[];
            return links.some((candidate) => (candidate.textContent ?? '').trim() === targetLabel);
          }, label),
          { timeout: 5000, timeoutMsg: `Expected assistant link to render: ${label}` },
        );
      } catch (_err) {
        const debug = await browser.execute(() => {
          const messages = Array.from(document.querySelectorAll('.assistant-message .message-content'))
            .map((node) => ({
              text: node.textContent ?? '',
              html: (node as HTMLElement).innerHTML,
            }));
          const links = Array.from(document.querySelectorAll('.assistant-message a'))
            .map((node) => ({
              text: node.textContent ?? '',
              href: (node as HTMLAnchorElement).getAttribute('href') ?? '',
            }));
          return { messages, links };
        });
        throw new Error(`Expected assistant link to render: ${label}. Debug: ${JSON.stringify(debug)}`);
      }

      await browser.execute((targetLabel: string) => {
        const links = Array.from(document.querySelectorAll('.assistant-message a')) as HTMLAnchorElement[];
        const link = links.find((candidate) => (candidate.textContent ?? '').trim() === targetLabel);
        if (!link) throw new Error(`Could not find assistant link: ${targetLabel}`);
        link.click();
      }, label);
    };

    await clickAssistantLink('agent_mcp_http.rs:250');

    await browser.waitUntil(
      async () => browser.execute((activeSel: string, titleSel: string) => {
        const active = document.querySelector(activeSel) as HTMLElement | null;
        if (!active) return false;
        const title = active.querySelector(titleSel) as HTMLElement | null;
        return (title?.textContent ?? '').trim() === 'agent_mcp_http.rs';
      }, sel.convTabActive, sel.convTabTitle),
      { timeout: 5000, timeoutMsg: 'Expected file tab agent_mcp_http.rs to open from chat link' },
    );

    await browser.waitUntil(
      async () => browser.execute(() => {
        const nums = Array.from(document.querySelectorAll('.diff-panel-file-line .diff-panel-linenum'));
        return nums.some((node) => (node.textContent ?? '').trim() === '250');
      }),
      { timeout: 5000, timeoutMsg: 'Expected linked line number to be visible in the file viewer' },
    );

    await browser.execute((tabSel: string) => {
      const tabs = Array.from(document.querySelectorAll(tabSel)) as HTMLElement[];
      const chatTab = tabs.find((tab) => !tab.classList.contains('conv-tab-file'));
      if (!chatTab) throw new Error('Could not find chat tab to return to message view');
      chatTab.click();
    }, sel.convTab);

    await input.waitForDisplayed({ timeout: 5000 });
    await clickAssistantLink('settings.ts');

    await browser.waitUntil(
      async () => browser.execute((activeSel: string, titleSel: string) => {
        const active = document.querySelector(activeSel) as HTMLElement | null;
        if (!active) return false;
        const title = active.querySelector(titleSel) as HTMLElement | null;
        return (title?.textContent ?? '').trim() === 'settings.ts';
      }, sel.convTabActive, sel.convTabTitle),
      { timeout: 5000, timeoutMsg: 'Expected settings.ts file tab to open from chat link without line' },
    );

    await browser.waitUntil(
      async () => browser.execute(() => {
        const lines = Array.from(document.querySelectorAll('.diff-panel-file-line .diff-panel-line-text'));
        return lines.some((node) => (node.textContent ?? '').includes('settings value from mock'));
      }),
      { timeout: 5000, timeoutMsg: 'Expected file content to render after opening link without line' },
    );

    await browser.execute(() => {
      delete (window as any).__mockReadFileContentByPath;
    });
  });
});
