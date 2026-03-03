import { sel } from './helpers';

describe('Git panel - non-git project', () => {
  it('shows clean message instead of error for non-git projects', async () => {
    await browser.url('/');
    const app = await $(sel.app);
    await app.waitForExist({ timeout: 10_000 });

    // Set flag BEFORE clicking open so the first git refresh sees it
    await browser.execute(() => {
      (window as any).__mockGitNotRepo = true;
    });

    const openBtn = await $(sel.openWorkspaceBtn);
    await openBtn.waitForClickable({ timeout: 10_000 });
    await openBtn.click();

    const title = await $(sel.appTitle);
    await browser.waitUntil(
      async () => (await title.getText()).includes('workspace'),
      { timeout: 10_000, timeoutMsg: 'Workspace did not load' },
    );

    // Activate the git widget tab since 'files' is active by default
    const gitTab = await $(sel.dockWidgetTab + '[data-widget="git"]');
    await gitTab.waitForExist({ timeout: 5_000 });
    await gitTab.click();

    const gitEmpty = await $(sel.gitEmpty);
    await gitEmpty.waitForExist({ timeout: 5_000 });
    expect(await gitEmpty.getText()).toBe('Not a git repository');

    const errorNotification = await $(sel.notificationError);
    expect(await errorNotification.isExisting()).toBe(false);
  });
});
