import { resetAppState, sel } from './helpers';

const MOCK_WORKFLOWS = [
  {
    id: 'wf-build',
    name: 'Build Project',
    description: 'Runs the build',
    trigger: '/build',
    steps: [
      {
        name: 'Install deps',
        actions: [{ type: 'run_command', command: 'npm install' }],
      },
      {
        name: 'Build',
        actions: [{ type: 'run_command', command: 'npm run build' }],
      },
    ],
    scope: 'project',
  },
  {
    id: 'wf-lint',
    name: 'Lint Code',
    description: 'Runs the linter',
    trigger: '/lint',
    steps: [
      {
        name: 'Lint',
        actions: [{ type: 'run_command', command: 'npm run lint' }],
      },
    ],
    scope: 'global',
  },
];

async function openWorkspaceWithWorkflows(): Promise<void> {
  await resetAppState();

  // Set mock workflows before opening the workspace so store.load() picks them up
  await browser.execute((workflows) => {
    (window as any).__mockWorkflows = workflows;
  }, MOCK_WORKFLOWS);

  const app = await $(sel.app);
  await app.waitForExist({ timeout: 10_000 });

  const openWorkspaceBtn = await $(sel.openWorkspaceBtn);
  await openWorkspaceBtn.waitForClickable({ timeout: 10_000 });
  await openWorkspaceBtn.click();

  const title = await $(sel.appTitle);
  await browser.waitUntil(
    async () => (await title.getText()).includes('workspace'),
    { timeout: 10_000, timeoutMsg: 'Workspace did not load' },
  );

  // Wait for the workflow store to finish loading
  await browser.pause(500);
}

describe('Workflows panel', () => {
  it('comprehensive workflow flow: empty state → run → expand → manager → hide completed', async () => {
    await openWorkspaceWithWorkflows();

    // --- 1. Switch to Workflows tab in the right dock ---
    const workflowsTab = await $(
      '[data-testid="dock-widget-tab"][data-widget="workflows"]',
    );
    await workflowsTab.waitForExist({ timeout: 5000 });
    await workflowsTab.click();
    await browser.pause(300);

    // --- 2. Verify empty state (no runs yet) ---
    const emptyState = await browser.execute(() => {
      const panel = document.querySelector('.workflows-panel');
      if (!panel) return { exists: false, emptyVisible: false, emptyText: '' };
      const empty = panel.querySelector('.workflows-empty-state');
      return {
        exists: true,
        emptyVisible: !!empty,
        emptyText: empty?.textContent ?? '',
      };
    });
    expect(emptyState.exists).toBe(true);
    expect(emptyState.emptyVisible).toBe(true);
    expect(emptyState.emptyText).toContain('No workflow runs yet');

    // --- 3. Open Run dropdown and verify workflow names ---
    const runBtn = await $('.workflows-run-btn');
    await runBtn.waitForDisplayed({ timeout: 5000 });
    await runBtn.click();

    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          return document.querySelectorAll('.workflows-run-menu-item').length > 0;
        });
      },
      { timeout: 5000, timeoutMsg: 'Run dropdown menu did not appear' },
    );

    const menuItems = await browser.execute(() => {
      const items = document.querySelectorAll('.workflows-run-menu-item');
      return Array.from(items).map((el) => el.textContent ?? '');
    });
    expect(menuItems).toContain('Build Project');
    expect(menuItems).toContain('Lint Code');

    // --- 4. Run "Build Project" workflow ---
    await browser.execute(() => {
      const items = document.querySelectorAll('.workflows-run-menu-item');
      for (const item of items) {
        if (item.textContent === 'Build Project') {
          (item as HTMLElement).click();
          return;
        }
      }
    });

    // Wait for the run card to appear and complete
    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          const cards = document.querySelectorAll('.workflow-run-card');
          return (
            cards.length > 0 &&
            cards[0].classList.contains('workflow-run-card-completed')
          );
        });
      },
      { timeout: 10_000, timeoutMsg: 'Workflow run did not complete' },
    );

    // Verify the run card shows correct info
    const cardInfo = await browser.execute(() => {
      const card = document.querySelector('.workflow-run-card');
      if (!card) return { title: '', summary: '' };
      const title =
        card.querySelector('.workflow-run-card-title')?.textContent ?? '';
      const summary =
        card.querySelector('.workflow-run-card-summary')?.textContent ?? '';
      return { title, summary };
    });
    expect(cardInfo.title).toBe('Build Project');
    expect(cardInfo.summary).toContain('2 steps completed');

    // --- 5. Run card auto-expands — verify action cards are visible ---
    const actionDetails = await browser.execute(() => {
      const detail = document.querySelector('.workflow-run-detail');
      if (!detail)
        return {
          visible: false,
          actionTitles: [] as string[],
          actionCount: 0,
        };
      const cards = detail.querySelectorAll('.workflow-action-card');
      const titles = Array.from(cards).map(
        (c) => c.querySelector('.workflow-action-card-title')?.textContent ?? '',
      );
      return {
        visible: true,
        actionTitles: titles,
        actionCount: cards.length,
      };
    });
    expect(actionDetails.visible).toBe(true);
    expect(actionDetails.actionCount).toBe(2);
    expect(actionDetails.actionTitles).toContain('npm install');
    expect(actionDetails.actionTitles).toContain('npm run build');

    // Elapsed time is in the card footer
    const footerTime = await browser.execute(() => {
      const footer = document.querySelector('.workflow-run-card-time');
      return footer?.textContent ?? '';
    });
    expect(footerTime).toContain('\u00B7');

    // Verify action cards show completed status (green left border)
    const actionStatuses = await browser.execute(() => {
      const cards = document.querySelectorAll('.workflow-action-card');
      return Array.from(cards).map((c) => ({
        completed: c.classList.contains('workflow-action-card-completed'),
      }));
    });
    for (const status of actionStatuses) {
      expect(status.completed).toBe(true);
    }

    // --- 6. Verify action output is visible in completed action cards ---
    const actionOutput = await browser.execute(() => {
      const output = document.querySelector('.workflow-run-step-output');
      return output?.textContent ?? '';
    });
    expect(actionOutput).toContain('mock output for');

    // Collapse run detail by clicking the card
    await browser.execute(() => {
      const card = document.querySelector('.workflow-run-card');
      if (card) (card as HTMLElement).click();
    });
    await browser.pause(200);

    const detailGone = await browser.execute(() => {
      return !document.querySelector('.workflow-run-detail');
    });
    expect(detailGone).toBe(true);

    // Re-expand by clicking again
    await browser.execute(() => {
      const card = document.querySelector('.workflow-run-card');
      if (card) (card as HTMLElement).click();
    });
    await browser.pause(200);

    const detailBack = await browser.execute(() => {
      return !!document.querySelector('.workflow-run-detail');
    });
    expect(detailBack).toBe(true);

    // Collapse again for clean state before next test
    await browser.execute(() => {
      const card = document.querySelector('.workflow-run-card');
      if (card) (card as HTMLElement).click();
    });
    await browser.pause(200);

    // --- 7. Run "Lint Code" workflow (second run) ---
    await browser.execute(() => {
      const btn = document.querySelector('.workflows-run-btn');
      if (btn) (btn as HTMLElement).click();
    });

    // Wait for the dropdown menu to appear
    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          return document.querySelectorAll('.workflows-run-menu-item').length > 0;
        });
      },
      { timeout: 5000, timeoutMsg: 'Run dropdown menu did not appear' },
    );

    await browser.execute(() => {
      const items = document.querySelectorAll('.workflows-run-menu-item');
      for (const item of items) {
        if (item.textContent === 'Lint Code') {
          (item as HTMLElement).click();
          return;
        }
      }
    });

    // Wait for second run to complete
    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          const cards = document.querySelectorAll('.workflow-run-card');
          return (
            cards.length >= 2 &&
            Array.from(cards).every((c) =>
              c.classList.contains('workflow-run-card-completed'),
            )
          );
        });
      },
      { timeout: 10_000, timeoutMsg: 'Second workflow run did not complete' },
    );

    // Verify 2 run cards, sorted newest first
    const runCards = await browser.execute(() => {
      const cards = document.querySelectorAll('.workflow-run-card');
      return Array.from(cards).map(
        (c) => c.querySelector('.workflow-run-card-title')?.textContent ?? '',
      );
    });
    expect(runCards.length).toBe(2);
    expect(runCards[0]).toBe('Lint Code');
    expect(runCards[1]).toBe('Build Project');

    // --- 8. Test the gear button → Manage Workflows overlay ---
    const gearBtn = await $(
      '.workflows-toolbar-btn[title="Manage workflows"]',
    );
    await gearBtn.waitForDisplayed({ timeout: 5000 });
    await gearBtn.click();
    await browser.pause(500);

    const managerInfo = await browser.execute(() => {
      const overlay = document.querySelector('.workflow-builder-overlay');
      if (!overlay || overlay.classList.contains('hidden')) {
        return { visible: false, title: '', workflowNames: [] as string[] };
      }
      const title =
        overlay.querySelector('.workflow-builder-header h2')?.textContent ?? '';
      const rows = overlay.querySelectorAll('.workflow-manager-row');
      const names = Array.from(rows).map(
        (r) =>
          r.querySelector('.workflow-manager-name')?.textContent ?? '',
      );
      return { visible: true, title, workflowNames: names };
    });
    expect(managerInfo.visible).toBe(true);
    expect(managerInfo.title).toContain('Manage Workflows');
    expect(managerInfo.workflowNames).toContain('Build Project');
    expect(managerInfo.workflowNames).toContain('Lint Code');

    // Close the manager overlay
    await browser.execute(() => {
      const closeBtn = document.querySelector('.workflow-builder-close');
      if (closeBtn) (closeBtn as HTMLElement).click();
    });
    await browser.pause(300);

    const overlayHidden = await browser.execute(() => {
      const overlay = document.querySelector('.workflow-builder-overlay');
      return !overlay || overlay.classList.contains('hidden');
    });
    expect(overlayHidden).toBe(true);

    // --- 9. Test "Hide completed" toggle ---
    const hideBtn = await $(
      '.workflows-toolbar-btn[title="Hide completed runs"]',
    );
    await hideBtn.waitForDisplayed({ timeout: 5000 });
    await hideBtn.click();
    await browser.pause(300);

    // All runs are completed, so hiding them shows empty state
    const hiddenState = await browser.execute(() => {
      const cards = document.querySelectorAll('.workflow-run-card');
      const empty = document.querySelector('.workflows-empty-state');
      return { cardCount: cards.length, emptyVisible: !!empty };
    });
    expect(hiddenState.cardCount).toBe(0);
    expect(hiddenState.emptyVisible).toBe(true);

    // Toggle button should be active
    const toggleActive = await browser.execute(() => {
      const btns = document.querySelectorAll('.workflows-toolbar-btn');
      for (const btn of btns) {
        if ((btn as HTMLElement).title === 'Hide completed runs') {
          return btn.classList.contains('workflows-toolbar-btn-active');
        }
      }
      return false;
    });
    expect(toggleActive).toBe(true);

    // Un-toggle → runs reappear
    await hideBtn.click();
    await browser.pause(300);

    const unhiddenCards = await browser.execute(() => {
      return document.querySelectorAll('.workflow-run-card').length;
    });
    expect(unhiddenCards).toBe(2);

    // --- 10. Test a failing workflow ---
    await browser.execute(() => {
      (window as any).__mockShellCommandHandler = (cmd: string) => {
        if (cmd.includes('lint')) {
          return {
            stdout: '',
            stderr: 'lint error: unexpected token',
            exit_code: 1,
            success: false,
          };
        }
        return {
          stdout: `mock output for: ${cmd}\n`,
          stderr: '',
          exit_code: 0,
          success: true,
        };
      };
    });

    // Run the lint workflow which will now fail
    await browser.execute(() => {
      const btn = document.querySelector('.workflows-run-btn');
      if (btn) (btn as HTMLElement).click();
    });

    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          return document.querySelectorAll('.workflows-run-menu-item').length > 0;
        });
      },
      { timeout: 5000, timeoutMsg: 'Run dropdown menu did not appear for failure test' },
    );

    await browser.execute(() => {
      const items = document.querySelectorAll('.workflows-run-menu-item');
      for (const item of items) {
        if (item.textContent === 'Lint Code') {
          (item as HTMLElement).click();
          return;
        }
      }
    });

    // Wait for the failed run card to appear
    await browser.waitUntil(
      async () => {
        return browser.execute(() => {
          const cards = document.querySelectorAll('.workflow-run-card');
          return (
            cards.length >= 3 &&
            cards[0].classList.contains('workflow-run-card-failed')
          );
        });
      },
      { timeout: 10_000, timeoutMsg: 'Failed workflow run did not appear' },
    );

    // Verify the failed card summary
    const failedCard = await browser.execute(() => {
      const card = document.querySelector('.workflow-run-card-failed');
      if (!card) return { title: '', summary: '' };
      return {
        title:
          card.querySelector('.workflow-run-card-title')?.textContent ?? '',
        summary:
          card.querySelector('.workflow-run-card-summary')?.textContent ?? '',
      };
    });
    expect(failedCard.title).toBe('Lint Code');
    expect(failedCard.summary).toContain('Failed at');

    // Failed run auto-expands — verify error detail is visible
    const failedDetail = await browser.execute(() => {
      const detail = document.querySelector('.workflow-run-detail');
      if (!detail)
        return { hasError: false, errorText: '', failedActionVisible: false };
      const errorEl = detail.querySelector('.workflow-run-error');
      const failedAction = detail.querySelector('.workflow-action-card-error');
      return {
        hasError: !!errorEl,
        errorText: errorEl?.textContent ?? '',
        failedActionVisible: !!failedAction,
      };
    });
    expect(failedDetail.hasError).toBe(true);
    expect(failedDetail.errorText).toContain('Error');
    expect(failedDetail.failedActionVisible).toBe(true);

    // --- Cleanup ---
    await browser.execute(() => {
      delete (window as any).__mockWorkflows;
      delete (window as any).__mockShellCommandHandler;
    });
  });
});
