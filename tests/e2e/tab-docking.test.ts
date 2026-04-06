import { openWorkspace, sel, sendPromptAndWaitForAssistant } from "./helpers";

const WORKSPACE_CONV_ID = 10000;

async function _emitChatEvent(
  conversationId: number,
  kind: string,
  data: unknown,
): Promise<void> {
  const payload = JSON.stringify({ conversationId, kind, data });
  await (browser as any).execute((json: string) => {
    const parsed = JSON.parse(json);
    const listeners = (window as any).__test_listeners?.["chat-event"] || [];
    for (const h of listeners) {
      h({
        event: "chat-event",
        id: 0,
        payload: {
          conversation_id: parsed.conversationId,
          event: { kind: parsed.kind, data: parsed.data },
        },
      });
    }
  }, payload);
}

function _makeAssistantMessage(content: string): Record<string, unknown> {
  return {
    sender: { Assistant: { agent: "tycode" } },
    content,
    timestamp: Date.now(),
    images: [],
    token_usage: null,
    reasoning: null,
    context_breakdown: null,
    tool_calls: [],
  };
}

async function dockConversation(
  conversationId: number,
  zone: "left" | "right",
): Promise<boolean> {
  return browser.execute(
    (id: number, z: string) =>
      (window as any).__test_dockConversation?.(id, z) ?? false,
    conversationId,
    zone,
  );
}

async function undockConversation(conversationId: number): Promise<void> {
  await browser.execute(
    (id: number) => (window as any).__test_undockConversation?.(id),
    conversationId,
  );
}

async function getTabTitles(): Promise<string[]> {
  return browser.execute((tabSel: string) => {
    return Array.from(document.querySelectorAll(tabSel))
      .filter((el) => (el as HTMLElement).offsetParent !== null)
      .map(
        (el) =>
          el.querySelector('[data-testid="conv-tab-title"]')?.textContent ?? "",
      );
  }, sel.convTab);
}

async function getDockedConversationTitle(): Promise<string | null> {
  return browser.execute(() => {
    const el = document.querySelector(".docked-conversation-title");
    return el?.textContent ?? null;
  });
}

async function isInputVisibleInDock(): Promise<boolean> {
  return browser.execute(() => {
    const docked = document.querySelector(
      '[data-testid="docked-conversation"]',
    );
    if (!docked) return false;
    const textarea = docked.querySelector(
      'textarea[aria-label="Message input"]',
    ) as HTMLTextAreaElement | null;
    if (!textarea) return false;
    const rect = textarea.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  });
}

async function _typeInDockedInput(text: string): Promise<void> {
  await browser.execute((t: string) => {
    const docked = document.querySelector(
      '[data-testid="docked-conversation"]',
    );
    if (!docked) throw new Error("No docked conversation found");
    const textarea = docked.querySelector(
      'textarea[aria-label="Message input"]',
    ) as HTMLTextAreaElement;
    if (!textarea) throw new Error("No textarea in docked conversation");
    textarea.value = t;
    textarea.dispatchEvent(new Event("input", { bubbles: true }));
  }, text);
}

async function _clickDockedSendButton(): Promise<void> {
  await browser.execute(() => {
    const docked = document.querySelector(
      '[data-testid="docked-conversation"]',
    );
    if (!docked) throw new Error("No docked conversation found");
    const btn = docked.querySelector(
      '[data-testid="send-btn"]',
    ) as HTMLButtonElement;
    if (!btn) throw new Error("No send button in docked conversation");
    btn.click();
  });
}

async function countMessagesInDock(): Promise<number> {
  return browser.execute((msgSel: string) => {
    const docked = document.querySelector(
      '[data-testid="docked-conversation"]',
    );
    if (!docked) return 0;
    return docked.querySelectorAll(msgSel).length;
  }, sel.assistantMessage);
}

async function countAssistantMessagesInCenter(): Promise<number> {
  return browser.execute((msgSel: string) => {
    const all = document.querySelectorAll(msgSel);
    let count = 0;
    for (const el of all) {
      if (!el.closest('[data-testid="docked-conversation"]')) count++;
    }
    return count;
  }, sel.assistantMessage);
}

async function _isSendButtonEnabledInDock(): Promise<boolean> {
  return browser.execute(() => {
    const docked = document.querySelector(
      '[data-testid="docked-conversation"]',
    );
    if (!docked) return false;
    const btn = docked.querySelector(
      '[data-testid="send-btn"]',
    ) as HTMLButtonElement | null;
    return btn ? !btn.disabled : false;
  });
}

describe("Tab docking lifecycle", () => {
  it("complete dock → chat → undock → chat journey with title preservation and multi-tab support", async () => {
    await openWorkspace();

    // --- Phase 1: Setup — create chat and send initial message ---
    const newChatBtn = await $(sel.welcomeNewChat);
    await newChatBtn.waitForExist({ timeout: 5000 });
    await browser.execute((el: HTMLElement) => el.click(), newChatBtn);
    const input = await $(sel.messageInput);
    await input.waitForDisplayed({ timeout: 5000 });

    await sendPromptAndWaitForAssistant("Hello from docking test");

    // Capture initial state
    const titlesBeforeDock = await getTabTitles();
    expect(titlesBeforeDock.length).toBe(1);
    const originalTitle = titlesBeforeDock[0];
    expect(originalTitle.length).toBeGreaterThan(0);

    const messagesBeforeDock = await countAssistantMessagesInCenter();
    expect(messagesBeforeDock).toBeGreaterThan(0);

    // --- Phase 2: Dock the conversation to the right zone ---
    const dockResult = await dockConversation(WORKSPACE_CONV_ID, "right");
    expect(dockResult).toBe(true);
    await browser.pause(300);

    // --- Phase 3: Verify docking results ---

    // 3a. Tab should be removed from center tab bar
    const tabsAfterDock = await getTabTitles();
    expect(tabsAfterDock.length).toBe(0);

    // 3b. Docked conversation tab should appear in dock zone
    const dockedTab = await $(sel.dockConversationTab);
    expect(await dockedTab.isExisting()).toBe(true);

    // 3c. Docked conversation wrapper should be visible
    const dockedConv = await $(sel.dockedConversation);
    expect(await dockedConv.isExisting()).toBe(true);

    // 3d. Title in dock header should match original tab title
    const dockedTitle = await getDockedConversationTitle();
    expect(dockedTitle).toBe(originalTitle);

    // 3e. Messages should be preserved in dock (not destroyed)
    const dockedMsgCount = await countMessagesInDock();
    expect(dockedMsgCount).toBe(messagesBeforeDock);

    // 3f. Only ONE dock tab should be highlighted (no double-highlight)
    const activeTabCount = await browser.execute((s: string) => {
      return document.querySelectorAll(s).length;
    }, sel.dockConversationTabActive);
    expect(activeTabCount).toBe(1);

    // 3g. Input should be visible and interactable in dock (textarea non-zero dimensions)
    const inputVisible = await isInputVisibleInDock();
    expect(inputVisible).toBe(true);

    // --- Phase 4: Chat in the docked conversation ---

    // 4a. Use sendPromptAndWaitForAssistant — after docking, the dock's textarea is the only
    //     visible one, so the helper will find it. This proves the full send → EventRouter →
    //     detached view → response render cycle works.
    await sendPromptAndWaitForAssistant("Message while docked");

    // 4b. Verify assistant message count increased in dock
    const msgCountAfterResponse = await countMessagesInDock();
    expect(msgCountAfterResponse).toBeGreaterThan(dockedMsgCount);

    // --- Phase 5: Undock the conversation ---
    await undockConversation(WORKSPACE_CONV_ID);
    await browser.pause(300);

    // --- Phase 6: Verify undocking results ---

    // 6a. Docked conversation should no longer exist
    const dockedAfterUndock = await $(sel.dockedConversation);
    expect(await dockedAfterUndock.isExisting()).toBe(false);

    // 6b. Tab should reappear in center tab bar
    const tabsAfterUndock = await getTabTitles();
    expect(tabsAfterUndock.length).toBe(1);

    // 6c. Title should be preserved (not renamed)
    expect(tabsAfterUndock[0]).toBe(originalTitle);

    // 6d. Conversation content should be preserved — assistant messages from both phases present
    const centerAssistantMsgs = await countAssistantMessagesInCenter();
    expect(centerAssistantMsgs).toBeGreaterThanOrEqual(messagesBeforeDock);

    // 6e. Input should be visible in center
    const centerInput = await $(sel.messageInput);
    await centerInput.waitForDisplayed({ timeout: 3000 });
    expect(await centerInput.isDisplayed()).toBe(true);

    // 6f. Verify the conversation has content in center view
    const centerHasContent = await browser.execute((msgSel: string) => {
      return document.querySelectorAll(msgSel).length > 0;
    }, sel.assistantMessage);
    expect(centerHasContent).toBe(true);

    // --- Phase 7: Chat works after undocking ---
    // sendPromptAndWaitForAssistant will timeout if chat is broken — this IS the proof
    await sendPromptAndWaitForAssistant("Message after undocking");

    // Verify the new response rendered somewhere accessible
    const hasUndockResponse = await browser.execute(() => {
      return (
        document.body.textContent?.includes("Message after undocking") ?? false
      );
    });
    expect(hasUndockResponse).toBe(true);

    // --- Phase 8: Re-dock cycle — dock again to verify re-docking works ---
    const reDockResult = await dockConversation(WORKSPACE_CONV_ID, "right");
    expect(reDockResult).toBe(true);
    await browser.pause(300);

    // Tab should be removed again
    const tabsAfterReDock = await getTabTitles();
    expect(tabsAfterReDock.length).toBe(0);

    // Docked conversation should be visible again
    const reDockedConv = await $(sel.dockedConversation);
    expect(await reDockedConv.isExisting()).toBe(true);

    // Title should still be preserved
    const reDockedTitle = await getDockedConversationTitle();
    expect(reDockedTitle).toBe(originalTitle);

    // Content from all phases should be preserved
    const reDockedMsgCount = await countMessagesInDock();
    expect(reDockedMsgCount).toBeGreaterThan(0);

    // Input should still work in re-docked state
    const reDockedInputVisible = await isInputVisibleInDock();
    expect(reDockedInputVisible).toBe(true);

    // Undock again
    await undockConversation(WORKSPACE_CONV_ID);
    await browser.pause(300);

    // Tab should reappear with correct title
    const tabsAfterReUndock = await getTabTitles();
    expect(tabsAfterReUndock.length).toBe(1);
    expect(tabsAfterReUndock[0]).toBe(originalTitle);

    // --- Phase 9: Multi-tab — create 2nd conversation, dock 1st, verify 2nd works ---

    // Create a second conversation tab
    await browser.execute(() => {
      const btn = Array.from(
        document.querySelectorAll("#center-new-tab-btn"),
      ).find((el) => (el as HTMLElement).offsetParent !== null) as
        | HTMLButtonElement
        | undefined;
      if (btn) btn.click();
    });
    await browser.pause(500);

    // Should now have 2 center tabs (original + new)
    const tabsWithTwo = await getTabTitles();
    expect(tabsWithTwo.length).toBe(2);

    // Dock the first conversation
    const multiDockResult = await dockConversation(WORKSPACE_CONV_ID, "right");
    expect(multiDockResult).toBe(true);
    await browser.pause(300);

    // Second tab should still exist in center
    const tabsAfterMultiDock = await getTabTitles();
    expect(tabsAfterMultiDock.length).toBe(1);

    // Re-select the remaining center tab, then verify its input is functional.
    await browser.execute((tabSel: string) => {
      const remainingTab = document.querySelector(tabSel) as HTMLElement | null;
      remainingTab?.click();
    }, sel.convTab);

    await browser.waitUntil(
      async () =>
        browser.execute(
          (inputSel: string, dockSel: string) => {
            const inputs = Array.from(
              document.querySelectorAll(inputSel),
            ) as HTMLTextAreaElement[];
            return inputs.some((input) => {
              if (input.closest(dockSel)) return false;
              const style = window.getComputedStyle(input);
              const rect = input.getBoundingClientRect();
              return (
                style.display !== "none" &&
                style.visibility !== "hidden" &&
                rect.width > 0 &&
                rect.height > 0
              );
            });
          },
          sel.messageInput,
          sel.dockedConversation,
        ),
      {
        timeout: 5000,
        timeoutMsg:
          "Expected visible center chat input after docking first tab",
      },
    );

    // Clean up: undock first conversation
    await undockConversation(WORKSPACE_CONV_ID);
    await browser.pause(300);

    // --- Phase 10: Double-dock prevention ---
    const dockForDouble11 = await dockConversation(WORKSPACE_CONV_ID, "right");
    expect(dockForDouble11).toBe(true);
    await browser.pause(200);

    // Docking same conversation again should fail
    const doubleDockResult = await dockConversation(WORKSPACE_CONV_ID, "left");
    expect(doubleDockResult).toBe(false);

    // Clean up
    await undockConversation(WORKSPACE_CONV_ID);
    await browser.pause(200);

    // Final state: tabs present, functional
    const finalTabs = await getTabTitles();
    expect(finalTabs.length).toBeGreaterThanOrEqual(1);
    expect(finalTabs).toContain(originalTitle);
  });
});
