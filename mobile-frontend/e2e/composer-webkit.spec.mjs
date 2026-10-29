import {expect, test} from '@playwright/test';

test('iOS standalone has no device recorder or diagnostics export', async ({page}) => {
  await page.setViewportSize({width:430,height:873});
  await page.addInitScript(() => {
    Object.defineProperty(navigator,'platform',{configurable:true,value:'iPhone'});
    Object.defineProperty(navigator,'standalone',{configurable:true,value:true});
  });
  const noCapture=async()=>{
    await expect(page.locator('[data-mobile-test=send-diagnostics-status], [data-mobile-test=send-diagnostics-panel]')).toHaveCount(0);
    await expect(page.getByLabel('Send diagnostics controls',{exact:true})).toHaveCount(0);
    expect(await page.evaluate(()=>typeof window.__TYDE_SEND_DIAGNOSTICS__)).toBe('undefined');
  };
  for(const legacyOptIn of [false,true]) {
    await page.goto('/?tyde-fixture=chat'+(legacyOptIn?'&tyde-send-diagnostics=1':''));
    await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
    await noCapture();
    const headerAccessible=async(renaming=false)=>{
      const result=await page.evaluate(renaming=>{
        const hit=selector=>{
          const element=document.querySelector(selector),r=element.getBoundingClientRect();
          return r.width>0 && r.height>0 && element.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));
        };
        return renaming
          ? hit('[data-mobile-test=chat-rename-input]') && hit('[data-mobile-test=chat-rename-cancel]')
          : hit('[data-mobile-test=chat-more]') && hit('[aria-label="Back to Agents"]');
      },renaming);
      expect(result,'chat header controls remain reachable without a diagnostics row').toBe(true);
    };
    await headerAccessible();
    const field=page.locator('[data-mobile-test=chat-input]');
    await field.focus();
    await page.evaluate(()=>{
      Object.defineProperty(visualViewport,'offsetTop',{configurable:true,value:386});
      Object.defineProperty(visualViewport,'height',{configurable:true,value:487});
      visualViewport.dispatchEvent(new Event('resize'));
    });
    await expect.poll(()=>page.evaluate(()=>Math.round(document.querySelector('.mobile-app').getBoundingClientRect().height))).toBe(487);
    await headerAccessible();
    await page.locator('[data-mobile-test=chat-more]').click();
    await page.locator('[data-mobile-test=chat-menu-rename]').click();
    await headerAccessible(true);
    await page.locator('[data-mobile-test=chat-rename-cancel]').click();
    await headerAccessible();
    await field.fill('Private draft');
    await noCapture();
    await page.locator('[data-mobile-test=chat-send]').click();
    await expect(field).toHaveValue('');
    expect(await page.evaluate(()=>window.__TYDE_FIXTURE_SENT_LINES__.length)).toBe(1);
    await noCapture();
    await page.getByRole('button',{name:'Back to Agents',exact:true}).click();
    await page.getByRole('tab',{name:'Settings',exact:true}).click();
    await noCapture();
    await expect(page.getByRole('button',{name:'Start new capture',exact:true})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Export',exact:true})).toHaveCount(0);
    await page.evaluate(()=>window.__TYDE_FIXTURE_HOST__(false));
    await noCapture();
  }
});


test('generated images open on a completed phone tap and retain the chat', async ({page}) => {
  await page.goto('/?tyde-fixture=images');
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  const thumbnail = page.getByRole('button', {name:'Open image full size'}).first();
  await thumbnail.scrollIntoViewIfNeeded();
  await thumbnail.locator('img').evaluate(image => image.decode());
  const input = page.locator('[data-mobile-test=chat-input]');
  await input.fill('Preserve the image draft');
  await input.focus();
  const before = await input.evaluate(field => {
    field.setSelectionRange(2,9);
    window.__imageFlowField = field;
    return {scroll:document.querySelector('[data-mobile-test=chat-messages]').scrollTop,
      scale:visualViewport.scale};
  });
  expect(await page.evaluate(() => {
    const app=document.querySelector('.mobile-app');
    return typeof app.onclick==='function' &&
      ['.mobile-app','.chat-messages','[data-mobile-test=chat-input]','.chat-message-image-button'].every(selector =>
        getComputedStyle(document.querySelector(selector)).touchAction==='manipulation');
  }), 'normal chat retains the landed WebKit double-tap protection').toBe(true);
  await thumbnail.tap();
  const viewer = page.getByRole('dialog', {name:'Image viewer'});
  await expect(viewer).toBeVisible();
  await expect(viewer.getByRole('button', {name:'Close image viewer'})).toBeVisible();
  await viewer.locator('img').evaluate(image => image.decode());
  expect(await viewer.evaluate(dialog => {
    const r=dialog.getBoundingClientRect();
    return dialog.matches(':modal') && r.width===innerWidth && r.height===innerHeight &&
      getComputedStyle(dialog.querySelector('.phone-image-stage')).touchAction==='none' &&
      getComputedStyle(dialog.querySelector('[data-image-close]')).touchAction==='manipulation';
  })).toBe(true);
  await viewer.getByRole('button', {name:'Zoom in'}).tap();
  await expect(viewer.getByRole('button', {name:'Reset zoom'})).toHaveText('150%');
  await viewer.getByRole('button', {name:'Next image'}).tap();
  await expect(viewer.getByRole('button', {name:'Reset zoom'})).toHaveText('100%');
  await page.evaluate(() => new Promise(resolve => {
    window.addEventListener('popstate', resolve, {once:true});
    history.back();
  }));
  await expect(viewer).toBeHidden();
  expect(await input.evaluate((field,before) => field===window.__imageFlowField &&
    field===document.activeElement && field.value==='Preserve the image draft' &&
    field.selectionStart===2 && field.selectionEnd===9 &&
    document.querySelector('[data-mobile-test=chat-messages]').scrollTop===before.scroll &&
    visualViewport.scale===before.scale, before)).toBe(true);
});
