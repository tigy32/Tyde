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

test('document reload preserves committed input and never replays uncertain submissions', async ({page}) => {
  const outbound = [];
  await page.exposeFunction('__TYDE_FIXTURE_OUTBOUND__', line => outbound.push(JSON.parse(line)));
  const open = async () => {
    await page.goto('/?tyde-fixture=recovery');
    await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  };
  const commit = () => page.evaluate(() => window.__TYDE_FIXTURE_COMMIT_INPUT__());
  const input = page.locator('[data-mobile-test=chat-input]');
  await page.goto('/?tyde-fixture=chat');
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  await input.fill('Uncommitted control draft');
  await page.reload();
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  expect(await input.inputValue() !== 'Uncommitted control draft', 'the old non-journaled boundary really loses draft memory').toBe(true);
  await open();
  const selectedHeading = await page.locator('[data-mobile-test=chat-title]').textContent();
  await input.fill('');
  await page.locator('[data-mobile-test=chat-photo-input]').setInputFiles({
    name:'draft.png',mimeType:'image/png',
    buffer:Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6lVQAAAAASUVORK5CYII=','base64'),
  });
  await expect(page.locator('[data-mobile-test=chat-photo-tray] img')).toHaveCount(1);
  await commit();
  await page.reload();
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  await expect(input).toHaveValue('');
  await expect(page.locator('[data-mobile-test=chat-photo-tray] img')).toHaveCount(1);
  expect(await page.locator('[data-mobile-test=chat-title]').textContent() === selectedHeading, 'exact selected conversation survives the document boundary').toBe(true);
  expect(outbound.length,'a fresh document restores input, never transmits it').toBe(0);
  await input.fill('Draft before document loss');
  await page.locator('[data-mobile-test=chat-send]').click();
  await expect(input).toHaveValue('');
  expect(outbound.length,'the explicit Send is the only transmitted request').toBe(1);
  await input.fill('Newer unsent draft');
  await commit();
  await page.reload();
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  await page.waitForFunction(expected => document.querySelector('[data-mobile-test=chat-input]').value === expected, 'Newer unsent draft');
  const held = page.locator('[data-mobile-test=pending-submission]');
  await expect(held).toHaveCount(1);
  await expect(held).toContainText('May not have been received');
  expect(await held.textContent().then(value => value.includes('Draft before document loss')), 'held custody retains the original input').toBe(true);
  expect(outbound.length,'unresolved custody is not replayed across fresh documents').toBe(1);
  await expect(page.locator('[data-mobile-test=chat-photo-tray] img')).toHaveCount(0);
  // Reject a real IndexedDB write, not the send assertion or transport recorder.
  await page.evaluate(() => {
    const put=IDBObjectStore.prototype.put;
    window.__restoreRecoveryWrites=()=>{IDBObjectStore.prototype.put=put;};
    IDBObjectStore.prototype.put=function(...args) {
      if(this.name==='paired_hosts' && typeof args[1]==='string' && args[1].startsWith('input-recovery:')) throw new DOMException('Injected storage failure','QuotaExceededError');
      return Reflect.apply(put,this,args);
    };
  });
  await input.fill('Retain on storage failure');
  await page.locator('[data-mobile-test=chat-send]').click();
  await expect(page.locator('[data-mobile-test=mobile-shell-error-banner]')).toBeVisible();
  expect(await input.inputValue() === 'Retain on storage failure', 'storage failure keeps input in the visible composer').toBe(true);
  expect(outbound.length,'uncommitted input is never transmitted').toBe(1);
  await page.evaluate(()=>{window.__documentBeforeRetry=document;});
  await page.locator('[data-mobile-test=recovery-retry]').click();
  await expect(page.locator('[data-mobile-test=mobile-shell-error-banner]')).toBeVisible();
  expect(await page.evaluate(()=>document===window.__documentBeforeRetry),'a failed persistence retry does not reload away unsaved input').toBe(true);
  expect(await input.inputValue()==='Retain on storage failure').toBe(true);
  await page.screenshot({path:test.info().outputPath('recovery-storage-error.png')});
  await page.evaluate(()=>window.__restoreRecoveryWrites());
  await page.locator('[data-mobile-test=recovery-retry]').click();
  await expect(page.locator('[data-mobile-test=mobile-shell-error-banner]')).toHaveCount(0);
  expect(await page.evaluate(()=>document===window.__documentBeforeRetry),'successful persistence resumes the same document').toBe(true);
  await commit();
  await page.reload();
  await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
  await page.waitForFunction(()=>document.querySelector('[data-mobile-test=chat-input]').value==='Retain on storage failure');
  expect(outbound.length,'Retry saves input but never retries delivery').toBe(1);
  await page.screenshot({path:test.info().outputPath('recovery-restored-custody.png')});
  await page.addInitScript(()=>{
    const keys=IDBObjectStore.prototype.getAllKeys;
    IDBObjectStore.prototype.getAllKeys=function(...args) {
      IDBObjectStore.prototype.getAllKeys=keys;
      throw new DOMException('Injected restore read failure','UnknownError');
    };
  });
  await page.reload();
  await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
  await expect(page.locator('[data-mobile-test=recovery-retry]')).toBeVisible();
  let opened;
  const openPending=new Promise(resolve=>{opened=resolve;});
  let releaseOpen;
  await page.exposeFunction('__holdRestoreOpen__',()=>{opened();return new Promise(resolve=>{releaseOpen=resolve;});});
  await page.evaluate(()=>{
    const open=IDBFactory.prototype.open;
    const success=Object.getOwnPropertyDescriptor(IDBRequest.prototype,'onsuccess');
    window.__retryOpenCount=0;
    IDBFactory.prototype.open=function(...args) {
      const request=Reflect.apply(open,this,args);
      ++window.__retryOpenCount;
      if(window.__retryOpenCount===1) Object.defineProperty(request,'onsuccess',{
        set(callback) {success.set.call(request,callback===null?null:event=>{window.__holdRestoreOpen__().then(()=>callback.call(request,event));});},
      });
      return request;
    };
  });
  await page.locator('[data-mobile-test=recovery-retry]').evaluate(button=>{button.click();button.click();});
  await openPending;
  expect(await page.evaluate(()=>window.__retryOpenCount),'a second Retry cannot start a concurrent restoration').toBe(1);
  releaseOpen();
  await expect(page.locator('[data-mobile-test=mobile-shell-error-banner]')).toHaveCount(0);
  await page.waitForFunction(()=>document.querySelector('[data-mobile-test=chat-input]').value==='Retain on storage failure');
});

test('queued input retains its original owner when another conversation has the same draft', async ({page}) => {
  let unblock;
  let admitted;
  const waiting = new Promise(resolve => { admitted=resolve; });
  const outbound=[];
  await page.exposeFunction('__TYDE_FIXTURE_BEFORE_ADMISSION__', () => {
    admitted();
    return new Promise(resolve => { unblock=resolve; });
  });
  await page.exposeFunction('__TYDE_FIXTURE_OUTBOUND__', line => { outbound.push(JSON.parse(line)); });
  try {
    await page.goto('/?tyde-fixture=recovery');
    await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__===true);
    const input=page.locator('[data-mobile-test=chat-input]');
    await input.fill('Same content, different owner');
    await page.evaluate(() => window.__TYDE_FIXTURE_COMMIT_INPUT__());
    await page.locator('[data-mobile-test=chat-send]').click();
    await waiting;
    await page.getByRole('button',{name:'Back to Agents',exact:true}).click();
    await page.getByRole('button',{name:'Open chat with Read-state sync',exact:true}).click();
    await input.fill('Same content, different owner');
    unblock();
    await page.waitForFunction(() => (window.__TYDE_FIXTURE_SENT_LINES__||[]).length===1);
    await page.evaluate(() => window.__TYDE_FIXTURE_COMMIT_INPUT__());
    await page.reload();
    await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__===true);
    await expect(page.locator('[data-mobile-test=chat-title]')).toHaveText('Read-state sync');
    await page.waitForFunction(() => document.querySelector('[data-mobile-test=chat-input]').value==='Same content, different owner');
    expect(outbound.length,'the new document did not replay the old request').toBe(1);
    let reject;
    let prepared;
    const atPrepared=new Promise(resolve=>{prepared=resolve;});
    let rejected;
    const atRejected=new Promise(resolve=>{rejected=resolve;});
    let finish;
    await page.exposeFunction('__TYDE_FIXTURE_AFTER_PREPARE__',()=>{prepared();return new Promise(resolve=>{reject=()=>resolve(true);});});
    await page.exposeFunction('__TYDE_FIXTURE_REJECTED__',()=>{rejected();return new Promise(resolve=>{finish=resolve;});});
    // The previous admission probe no longer blocks the second request.
    await page.evaluate(()=>{window.__TYDE_FIXTURE_BEFORE_ADMISSION__=undefined;});
    await input.fill('Original request rejected after durable custody');
    await page.locator('[data-mobile-test=chat-send]').click();
    await atPrepared;
    await input.fill('A newer draft at rejection');
    await page.evaluate(()=>window.__TYDE_FIXTURE_COMMIT_INPUT__());
    reject();
    await atRejected;
    // Lose the actual document before the actor returns to its caller: no later repair can save it.
    await page.reload();
    finish();
    await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
    await page.waitForFunction(()=>document.querySelector('[data-mobile-test=chat-input]').value==='A newer draft at rejection');
    // Recovery cards are conversation-scoped. Visit both actual chats rather
    // than counting another chat's deliberately invisible holder as lost.
    await expect(page.locator('[data-mobile-test=pending-submission]')).toHaveCount(1);
    await expect(page.locator('[data-mobile-test=pending-submission]')).toContainText('Original request rejected after durable custody');
    await expect(page.locator('[data-mobile-test=pending-submission]')).toContainText('Not sent');
    await page.getByRole('button',{name:'Back to Agents',exact:true}).click();
    await page.getByRole('button',{name:'Open chat with Mira',exact:true}).click();
    await expect(page.locator('[data-mobile-test=pending-submission]')).toHaveCount(1);
    await expect(page.locator('[data-mobile-test=pending-submission]')).toContainText('Same content, different owner');
    await expect(page.locator('[data-mobile-test=pending-submission]')).toContainText('May not have been received');
    expect(outbound.length,'a rejected frame never crossed the outbound boundary').toBe(1);
  } finally { unblock?.(); }
});

test('document loss during an explicit new-chat retry preserves one logical request and a newer draft', async ({page}) => {
  const outbound=[];
  let release;
  let reachedRetry;
  const retryCommitted=new Promise(resolve => { reachedRetry=resolve; });
  await page.exposeFunction('__TYDE_FIXTURE_OUTBOUND__', line => {
    outbound.push(JSON.parse(line));
    if(outbound.length===2) {
      reachedRetry();
      return new Promise(resolve => { release=resolve; });
    }
  });
  const readIdentity = () => page.evaluate(async () => {
    const db=await new Promise((resolve,reject) => {
      const request=indexedDB.open('tyde-mobile',1);
      request.onsuccess=()=>resolve(request.result);request.onerror=()=>reject(new Error('Storage open failed'));
    });
    const journal=await new Promise((resolve,reject) => {
      const tx=db.transaction('paired_hosts');
      const request=tx.objectStore('paired_hosts').get('input-recovery:'+JSON.stringify('fixture-host'));
      request.onsuccess=()=>resolve(JSON.parse(request.result));request.onerror=()=>reject(new Error('Storage read failed'));
    });
    db.close();
    return journal.pending.map(record=>({id:record.human_input_id,origin:record.origin}));
  });
  try {
    await page.goto('/?tyde-fixture=recovery');
    await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__===true);
    await page.getByRole('button',{name:'Back to Agents',exact:true}).click();
    await page.getByRole('button',{name:'New chat',exact:true}).click();
    await page.locator('[data-mobile-test=project-picker-none]').click();
    const input=page.locator('[data-mobile-test=chat-input]');
    await input.fill('Original new-chat request');
    await page.locator('[data-mobile-test=chat-send]').click();
    await expect(input).toHaveValue('');
    await page.evaluate(() => window.__TYDE_FIXTURE_COMMIT_INPUT__());
    const original=await readIdentity();
    await page.reload();
    await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__===true);
    await input.fill('A newer draft remains mine');
    await page.evaluate(() => window.__TYDE_FIXTURE_COMMIT_INPUT__());
    await page.locator('[data-mobile-test=pending-submission-resend]').click();
    await retryCommitted;
    // The complete custody transaction exists, but resend() has not retired its old in-memory attempt.
    await page.reload();
    await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__===true);
    await expect(page.locator('[data-mobile-test=pending-submission]')).toHaveCount(1);
    const recovered=await readIdentity();
    expect(original.length===1 && recovered.length===1 && original[0].id===recovered[0].id && original[0].origin===recovered[0].origin,
      'one original logical identity survives the interrupted explicit retry').toBe(true);
    await page.waitForFunction(() => document.querySelector('[data-mobile-test=chat-input]').value==='A newer draft remains mine');
    expect(outbound.length,'only the two explicit requests crossed the boundary').toBe(2);
    let rejected;
    const rejectionCommitted=new Promise(resolve=>{rejected=resolve;});
    let finishRejection;
    await page.exposeFunction('__TYDE_FIXTURE_AFTER_PREPARE__',()=>true);
    await page.exposeFunction('__TYDE_FIXTURE_REJECTED__',()=>{rejected();return new Promise(resolve=>{finishRejection=resolve;});});
    await page.locator('[data-mobile-test=pending-submission-resend]').click();
    await rejectionCommitted;
    await page.reload();
    finishRejection();
    await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
    await expect(page.locator('[data-mobile-test=pending-submission]')).toHaveCount(1);
    await expect(page.locator('[data-mobile-test=pending-submission]')).toContainText('May not have been received');
    await expect(page.locator('[data-mobile-test=pending-submission-resend-warning]')).toContainText('second agent');
    const afterRejection=await readIdentity();
    expect(afterRejection.length===1 && afterRejection[0].id===original[0].id && afterRejection[0].origin===original[0].origin,
      'a refused replacement preserves the original unresolved holder').toBe(true);
    expect(outbound.length,'rejected retry does not erase uncertainty or transmit a third request').toBe(2);
  } finally { release?.(); }
});
