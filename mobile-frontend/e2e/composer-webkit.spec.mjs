import {expect, test} from '@playwright/test';
import {readFile, writeFile} from 'node:fs/promises';
const {version:compiledVersion}=JSON.parse(await readFile(new URL('../../package.json',import.meta.url),'utf8'));
const compiledBeta=compiledVersion.includes('-beta.');
import {preflightScreenshotOutput} from '../../tools/devicefarm-output.mjs';

test('actual Tyde composer: synthetic composition, lifecycle, resize and hit roles', async ({page}, testInfo) => {
  await page.goto('/?tyde-fixture=chat&tyde-send-diagnostics=1');
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  const outputPreflight=await preflightScreenshotOutput(await page.screenshot());
  expect(outputPreflight.status).toBe('PASS');
  await writeFile(testInfo.outputPath('screenshot-output-preflight.json'),JSON.stringify(outputPreflight));
  const result = await page.evaluate(async () => {
    const field = document.querySelector('[data-mobile-test=chat-input]');
    const primary = document.querySelector('[data-mobile-test=chat-send]');
    const tick = ms => new Promise(resolve => setTimeout(resolve, ms));
    const input = (value, type = 'insertCompositionText', composing = true) => {
      field.value = value;
      field.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:type, isComposing:composing}));
    };
    const center = () => { const r = primary.getBoundingClientRect(); return [r.x+r.width/2,r.y+r.height/2]; };
    const hit = point => primary.contains(document.elementFromPoint(...point));
    const contact = (type, point) => primary.dispatchEvent(new PointerEvent(type, {bubbles:true,pointerType:'touch',clientX:point[0],clientY:point[1]}));
    const cases = [];
    let sent = 0;
    for (const scenario of ['composing-immediate','composing-tick','replacement-running','replacement-idle']) {
      input('existing draft', 'insertText', false); await tick(0);
      field.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
      input('composing draft');
      if (scenario !== 'composing-immediate') await tick(0);
      const point = center(); contact('pointerdown',point);
      let expected = 'composing draft';
      if (scenario.startsWith('replacement')) {
        window.__TYDE_FIXTURE_ACTIVITY__(scenario.endsWith('running'));
        input('replacement draft', 'insertReplacementText', false); expected = 'replacement draft';
        await tick(0);
      }
      const before = {nodeSame:primary===document.querySelector('[data-mobile-test=chat-send]'),connected:primary.isConnected,
        enabled:!primary.disabled,centerHit:hit(center()),originalHit:hit(point),action:primary.textContent.trim()};
      contact('pointerup',point); primary.click(); await tick(0); await tick(0);
      const lines = window.__TYDE_FIXTURE_SENT_LINES__ || [];
      const frame = lines.length === sent+1 ? JSON.parse(lines[sent]) : null;
      const cleared = field.value === '';
      field.dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
      field.dispatchEvent(new InputEvent('input',{bubbles:true,isComposing:false,inputType:'insertFromComposition'}));
      await tick(0);
      cases.push({scenario,...before,oneSend:frame?.kind==='send_message',payloadEqual:frame?.payload.message===expected,
        cleared,noDuplicate:(window.__TYDE_FIXTURE_SENT_LINES__||[]).length===sent+1,eventsSynthetic:true});
      sent++;
    }
    const resize = [];
    input('short','insertText',false); await tick(0);
    for (const delay of [0,50,250]) for (const value of ['wrapped words '.repeat(30),'short','line\n'.repeat(40),'short']) {
      const point = center(); contact('pointerdown',point);
      const before = field.getBoundingClientRect().height, start = performance.now();
      input(value,'insertText',false); if(delay) await tick(delay);
      const elapsed = performance.now()-start;
      const height = field.getBoundingClientRect().height;
      resize.push({delay,elapsed,height,changed:value==='short'?height<before:height>before,
        centerHit:hit(center()),originalHit:hit(point),nativeEqual:field.value===value,nodeSame:primary===document.querySelector('[data-mobile-test=chat-send]')});
      contact('pointerup',point);
    }
    const point=center();
    document.querySelector('[data-mobile-test=chat-send-menu-toggle]').click(); await tick(0);
    const backdrop=document.querySelector('[data-mobile-test=chat-send-menu-backdrop]');
    const openIntercept=document.elementFromPoint(...point)===backdrop;
    backdrop.click(); await tick(0);
    const closedHit=hit(point)&&hit(center())&&!backdrop.isConnected;
    return {cases,resize,openIntercept,closedHit};
  });
  await writeFile(testInfo.outputPath('sanitized-composer-probe.json'),JSON.stringify(result,null,2));
  await testInfo.attach('sanitized-composer-probe', {path:testInfo.outputPath('sanitized-composer-probe.json'),contentType:'application/json'});
  for (const row of result.cases) {
    expect(row.nodeSame && row.connected && row.enabled && row.centerHit && row.originalHit).toBe(true);
    expect(row.oneSend && row.payloadEqual && row.cleared && row.noDuplicate).toBe(true);
    expect(row.action).toBe(row.scenario==='replacement-running'?'Queue':'Send');
  }
  for (const row of result.resize) {
    expect(row.changed && row.centerHit && row.originalHit && row.nativeEqual && row.nodeSame).toBe(true);
    expect(row.elapsed).toBeLessThan(300);
  }
  expect(result.openIntercept && result.closedHit).toBe(true);
});


test('iOS standalone beta diagnostics capture by default and export a bounded private file', async ({page}, testInfo) => {
  await page.setViewportSize({width:430,height:873});
  await page.addInitScript(() => {
    Object.defineProperty(navigator,'platform',{configurable:true,value:'iPhone'});
    Object.defineProperty(navigator,'standalone',{configurable:true,value:true});
  });
  await page.goto('/?tyde-fixture=chat');
  await page.waitForFunction(() => window.__TYDE_FIXTURE_READY__ === true);
  if(compiledBeta) {
    await expect(page.locator('[data-mobile-test=send-diagnostics-status]')).toContainText('Recording');
  } else {
    await expect(page.locator('[data-mobile-test=send-diagnostics-status]')).toBeHidden();
    expect(await page.evaluate(()=>typeof window.__TYDE_SEND_DIAGNOSTICS__)).toBe('undefined');
  }
  // Full mounted app, not a direct false-configuration call on a leaf surface.
  await page.goto('/?tyde-fixture=chat&tyde-fixture-diagnostics-channel=stable');
  await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
  await expect(page.locator('[data-mobile-test=send-diagnostics-status]')).toBeHidden();
  expect(await page.evaluate(()=>typeof window.__TYDE_SEND_DIAGNOSTICS__)).toBe('undefined');
  await page.goto(compiledBeta?'/?tyde-fixture=chat':'/?tyde-fixture=chat&tyde-fixture-diagnostics-channel=beta');
  await page.waitForFunction(()=>window.__TYDE_FIXTURE_READY__===true);
  await expect(page.locator('[data-mobile-test=send-diagnostics-status]')).toContainText('Recording');
  await writeFile(testInfo.outputPath('diagnostic-channel-probe.json'),JSON.stringify({compiledBeta,automaticExpected:compiledBeta,forcedStableOff:true,testOwnedBetaForRecorder:!compiledBeta}));
  const headerResults=[];
  const headerAccessible=async(label,renaming=false)=>{
    await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
    const result=await page.evaluate(({label,renaming})=>{
      const panel=document.querySelector('[data-mobile-test=send-diagnostics-status]').getBoundingClientRect();
      const header=document.querySelector('.chat-header'),r=header.getBoundingClientRect();
      const hit=selector=>{
        const e=document.querySelector(selector);if(!e)return false;const b=e.getBoundingClientRect();
        return b.width>0&&b.height>0&&getComputedStyle(e).visibility==='visible'&&e.contains(document.elementFromPoint(b.x+b.width/2,b.y+b.height/2));
      };
      const selectors=renaming?['chat-rename-input','chat-rename-save','chat-rename-cancel']:['chat-title','chat-subtitle','chat-back','chat-more'];
      return {label,nonoverlap:panel.bottom<=r.top||r.bottom<=panel.top,headerHeight:r.height,panelBottom:panel.bottom,headerTop:r.top,
        readableAndHittable:selectors.every(name=>hit(`[data-mobile-test=${name}]`))};
    },{label,renaming});
    headerResults.push(result);
    await writeFile(testInfo.outputPath('diagnostic-header-probe.json'),JSON.stringify(headerResults,null,2));
    expect(result.nonoverlap&&result.readableAndHittable,'diagnostics must not obstruct title, subtitle or rename controls').toBe(true);
  };
  await headerAccessible('recording-closed');

  const disclosure=page.locator('[data-mobile-test=send-diagnostics-status] details');
  await page.getByLabel('Send diagnostics controls',{exact:true}).click();
  const globalPanel=page.locator('[data-mobile-test=send-diagnostics-global-panel]');
  await headerAccessible('recording-open');
  await globalPanel.getByRole('button',{name:'Stop',exact:true}).click();
  await headerAccessible('stopped-open');
  await globalPanel.getByRole('button',{name:'Clear',exact:true}).click();
  await page.evaluate(()=>window.__TYDE_FIXTURE_HOST__(false));
  await expect(globalPanel).toBeVisible();
  await expect(globalPanel).toContainText('Stopped');
  expect(await page.evaluate(()=>window.__TYDE_SEND_DIAGNOSTICS__.events.length)).toBe(0);
  await globalPanel.getByRole('button',{name:'Start new capture',exact:true}).click();
  await expect(globalPanel).toContainText('Recording');
  await page.evaluate(()=>window.__TYDE_FIXTURE_HOST__(true));
  await page.getByLabel('Send diagnostics controls',{exact:true}).click();
  await expect(disclosure).not.toHaveAttribute('open','');
  const field=page.locator('[data-mobile-test=chat-input]');
  await field.focus();
  // Replay the measured device viewport projection, not a native keyboard.
  await page.evaluate(()=>{
    const viewport=visualViewport;
    const top=Object.getOwnPropertyDescriptor(viewport,'offsetTop'),height=Object.getOwnPropertyDescriptor(viewport,'height');
    Object.defineProperty(viewport,'offsetTop',{configurable:true,value:386});
    Object.defineProperty(viewport,'height',{configurable:true,value:487});
    window.__restoreDiagnosticViewport=()=>{
      if(top)Object.defineProperty(viewport,'offsetTop',top);else delete viewport.offsetTop;
      if(height)Object.defineProperty(viewport,'height',height);else delete viewport.height;
      viewport.dispatchEvent(new Event('resize'));
      delete window.__restoreDiagnosticViewport;
    };
    viewport.dispatchEvent(new Event('resize'));
  });
  await expect.poll(()=>page.evaluate(()=>Math.round(document.querySelector('.mobile-app').getBoundingClientRect().height))).toBe(487);
  const projected=await page.evaluate(()=>{
    const shell=document.querySelector('.mobile-app').getBoundingClientRect();
    const summary=document.querySelector('[aria-label="Send diagnostics controls"]'),r=summary.getBoundingClientRect();
    return {inside:r.top>=shell.top&&r.bottom<=shell.bottom,hit:summary.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)),shellHeight:shell.height,offset:r.top-shell.top};
  });
  await writeFile(testInfo.outputPath('diagnostic-viewport-probe.json'),JSON.stringify(projected));
  expect(projected.inside && projected.hit,'diagnostic controls must stay in the projected shell after keyboard viewport shift').toBe(true);
  await headerAccessible('keyboard-recording-closed');
  await page.locator('[data-mobile-test=chat-more]').click();
  await page.locator('[data-mobile-test=chat-menu-rename]').click();
  await headerAccessible('keyboard-rename',true);
  await page.locator('[data-mobile-test=chat-rename-input]').fill('fixture rename draft');
  await page.locator('[data-mobile-test=chat-rename-cancel]').click();
  await headerAccessible('keyboard-rename-canceled');

  await page.getByLabel('Send diagnostics controls',{exact:true}).click();
  await globalPanel.getByRole('button',{name:'Stop',exact:true}).click();
  await globalPanel.getByRole('button',{name:'Start new capture',exact:true}).click();
  await page.getByLabel('Send diagnostics controls',{exact:true}).click();
  await page.evaluate(()=>{
    const original=Object.getOwnPropertyDescriptor(performance,'now'),now=performance.now.bind(performance);
    Object.defineProperty(performance,'now',{configurable:true,value:()=>now()+900001});
    document.dispatchEvent(new Event('visibilitychange'));
    if(original)Object.defineProperty(performance,'now',original);else delete performance.now;
  });
  await expect(page.locator('[data-mobile-test=send-diagnostics-status]')).toContainText('Expired');
  await headerAccessible('keyboard-expired-closed');
  await page.getByLabel('Send diagnostics controls',{exact:true}).click();
  await headerAccessible('keyboard-expired-open');
  await globalPanel.getByRole('button',{name:'Start new capture',exact:true}).click();
  await page.getByLabel('Send diagnostics controls',{exact:true}).click();
  await page.evaluate(()=>window.__restoreDiagnosticViewport());
  await field.fill('private sentinel that must not export');
  await page.locator('[data-mobile-test=chat-send]').click();
  await expect.poll(()=>field.evaluate(element=>element.value.length===0)).toBe(true);
  expect(await page.evaluate(()=>window.__TYDE_FIXTURE_SENT_LINES__.length)).toBe(1);
  await page.getByRole('button',{name:'Back to Agents',exact:true}).click();
  await page.getByRole('tab',{name:'Settings',exact:true}).click();
  const panel=page.locator('[data-mobile-test=send-diagnostics-panel]');
  await expect(panel).toContainText('Recording');
  const downloaded=page.waitForEvent('download');
  await panel.getByRole('button',{name:'Export',exact:true}).click();
  const file=await downloaded;
  const stream=await file.createReadStream();let text='';for await(const chunk of stream)text+=chunk;
  const exported=JSON.parse(text);
  expect(exported.schema).toBe(3);
  expect(exported.records.some(record=>record.phase==='admitted-locally')).toBe(true);
  await writeFile(testInfo.outputPath('sanitized-diagnostic-export.json'),text);
  expect(Buffer.byteLength(text)).toBeLessThanOrEqual(256*1024);
  expect(text.includes('private sentinel')).toBe(false);
  await panel.getByRole('button',{name:'Stop',exact:true}).click();
  await expect(panel).toContainText('Stopped');
  await panel.getByRole('button',{name:'Clear',exact:true}).click();
  expect(await page.evaluate(()=>window.__TYDE_SEND_DIAGNOSTICS__.events.length)).toBe(0);
  await panel.getByRole('button',{name:'Start new capture',exact:true}).click();
  await expect(panel).toContainText('Recording');
});
