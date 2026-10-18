// Dev/device injection only: never bundled, persisted, or sent over the network.
(() => {
  if (window.__tydeViewportProbe) throw new Error('Viewport probe already installed');
  const samples = [];
  const registrations = [];
  let frame = 0;
  const box = element => {
    if (!element) return null;
    const {x, y, width, height} = element.getBoundingClientRect();
    return {x, y, width, height};
  };
  const textStyle = element => {
    if (!element) return null;
    const style = getComputedStyle(element);
    return {fontSize: style.fontSize, lineHeight: style.lineHeight,
      textSizeAdjust: style.webkitTextSizeAdjust, transform: style.transform, zoom: style.zoom};
  };
  const capture = reason => {
    const active = document.activeElement;
    const root = document.querySelector('.mobile-app');
    const viewport = window.visualViewport;
    const sample = {
      reason, at: performance.now(), visibility: document.visibilityState,
      documentFocused: document.hasFocus(), standalone: navigator.standalone === true || matchMedia('(display-mode: standalone)').matches,
      coarsePointer: matchMedia('(any-pointer: coarse)').matches,
      viewport: viewport ? {scale: viewport.scale, width: viewport.width, height: viewport.height,
        offsetTop: viewport.offsetTop, offsetLeft: viewport.offsetLeft, pageTop: viewport.pageTop, pageLeft: viewport.pageLeft} : null,
      inner: {width: innerWidth, height: innerHeight}, outer: {width: outerWidth, height: outerHeight},
      client: {width: document.documentElement.clientWidth, height: document.documentElement.clientHeight},
      scroll: {x: scrollX, y: scrollY}, screen: {width: screen.width, height: screen.height},
      app: box(root), composer: box(document.querySelector('[data-mobile-test="chat-input"]')),
      active: active ? {tag: active.tagName, type: active instanceof HTMLInputElement ? active.type : null,
        bounds: box(active), text: textStyle(active)} : null,
      text: {html: textStyle(document.documentElement), body: textStyle(document.body), app: textStyle(root)},
      shell: root ? {top: root.style.getPropertyValue('--tws-top'), height: root.style.getPropertyValue('--tws-height'),
        keyboard: root.hasAttribute('data-tws-keyboard')} : null,
    };
    if (samples.length === 128) samples.shift();
    samples.push(sample);
    return sample;
  };
  const listen = (target, name) => {
    const callback = () => {
      capture(name);
      if (!frame) frame = requestAnimationFrame(() => { frame = 0; capture('frame'); });
    };
    target.addEventListener(name, callback, true);
    registrations.push([target, name, callback]);
  };
  for (const name of ['resize', 'orientationchange', 'pageshow', 'pagehide', 'focus', 'blur']) listen(window, name);
  for (const name of ['focusin', 'focusout', 'visibilitychange']) listen(document, name);
  if (window.visualViewport) for (const name of ['resize', 'scroll']) listen(visualViewport, name);
  window.__tydeViewportProbe = {samples, capture, stop() {
    if (frame) cancelAnimationFrame(frame);
    for (const [target, name, callback] of registrations) target.removeEventListener(name, callback, true);
    delete window.__tydeViewportProbe;
  }};
  capture('installed');
})();
