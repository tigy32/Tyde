import {expect, test} from '@playwright/test';
import {writeFile} from 'node:fs/promises';
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
