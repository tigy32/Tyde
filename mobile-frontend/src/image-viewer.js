// Interaction-only controller: image identity and bytes stay in the typed Rust gallery.
export function attachImageViewer(dialog) {
  const document = dialog.ownerDocument;
  const window = document.defaultView;
  const stage = dialog.querySelector('.phone-image-stage');
  const image = stage.querySelector('img');
  const resetButton = dialog.querySelector('[data-image-reset]');
  const zoomOut = dialog.querySelector('[data-image-zoom-out]');
  const zoomIn = dialog.querySelector('[data-image-zoom-in]');
  const error = dialog.querySelector('.phone-image-error');
  const token = window.crypto.randomUUID();
  const listeners = new AbortController();
  const options = { signal: listeners.signal };
  const pointers = new Map();
  let scale = 1, x = 0, y = 0;
  let gesture = null, lastTap = null;
  let moved = false;
  let returnFocus = null, selection = null, scroller = null, scrollTop = 0;
  let traversing = false;

  const ownsEntry = () => window.history.state?.tydeImageViewer === token;
  const paint = () => {
    const limitX = Math.max(0, (image.offsetWidth * scale - stage.clientWidth) / 2);
    const limitY = Math.max(0, (image.offsetHeight * scale - stage.clientHeight) / 2);
    x = Math.max(-limitX, Math.min(limitX, x));
    y = Math.max(-limitY, Math.min(limitY, y));
    image.style.transform = `translate(${x}px, ${y}px) scale(${scale})`;
    resetButton.textContent = `${Math.round(scale * 100)}%`;
    zoomOut.disabled = scale === 1;
    zoomIn.disabled = scale === 5;
    stage.style.cursor = scale > 1 ? 'grab' : 'zoom-in';
  };
  const reset = () => {
    scale = 1; x = 0; y = 0;
    pointers.clear(); gesture = null; lastTap = null; moved = false;
    paint();
  };
  const zoom = (next, clientX, clientY) => {
    const rect = stage.getBoundingClientRect();
    const anchorX = clientX - rect.left - rect.width / 2;
    const anchorY = clientY - rect.top - rect.height / 2;
    const target = Math.max(1, Math.min(5, next));
    x = anchorX - (anchorX - x) * target / scale;
    y = anchorY - (anchorY - y) * target / scale;
    scale = target;
    paint();
  };
  const zoomAtCenter = next => {
    const rect = stage.getBoundingClientRect();
    zoom(next, rect.left + rect.width / 2, rect.top + rect.height / 2);
  };
  const geometry = () => {
    const [a, b] = [...pointers.values()];
    return b ? { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2,
      distance: Math.hypot(a.x - b.x, a.y - b.y) } : { ...a, distance: 0 };
  };
  const rebase = () => {
    gesture = pointers.size ? { ...geometry(), scale, panX: x, panY: y } : null;
  };
  const restore = () => {
    if (returnFocus?.isConnected) {
      returnFocus.focus({ preventScroll: true });
      if (selection) returnFocus.setSelectionRange(...selection);
    }
    if (scroller?.isConnected) scroller.scrollTop = scrollTop;
  };
  const close = () => {
    if (!dialog.open || traversing) return;
    if (!ownsEntry()) throw new Error('Image viewer lost its navigation entry');
    traversing = true;
    window.history.back();
  };
  const show = () => {
    dialog.showModal();
    reset();
  };

  window.addEventListener('popstate', () => {
    traversing = false;
    if (ownsEntry()) {
      show();
    } else if (dialog.open) {
      dialog.close();
      reset();
      restore();
    }
  }, options);
  dialog.addEventListener('cancel', event => { event.preventDefault(); close(); }, options);
  dialog.querySelector('[data-image-close]').addEventListener('click', close, options);
  dialog.addEventListener('keydown', event => {
    if (event.key === 'Escape') {
      event.preventDefault(); event.stopPropagation(); close();
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
      event.preventDefault();
      dialog.querySelector(event.key === 'ArrowLeft' ? '[data-image-previous]' : '[data-image-next]').click();
    }
  }, options);
  dialog.addEventListener('wheel', event => event.preventDefault(), { ...options, passive: false });
  dialog.addEventListener('click', event => { if (event.target === dialog && !moved) close(); }, options);
  resetButton.addEventListener('click', reset, options);
  zoomOut.addEventListener('click', () => zoomAtCenter(scale / 1.5), options);
  zoomIn.addEventListener('click', () => zoomAtCenter(scale * 1.5), options);
  image.addEventListener('load', () => { error.hidden = true; reset(); }, options);
  image.addEventListener('error', () => { error.hidden = false; }, options);
  if (image.hasAttribute('src') && image.complete && !image.naturalWidth) error.hidden = false;
  image.addEventListener('dragstart', event => event.preventDefault(), options);
  stage.addEventListener('pointerdown', event => {
    if (!dialog.open || (event.pointerType === 'mouse' && event.button !== 0)) return;
    event.preventDefault();
    if (!pointers.size) moved = false;
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    if (pointers.size > 1) { moved = true; lastTap = null; }
    rebase();
  }, options);
  window.addEventListener('pointermove', event => {
    if (!pointers.has(event.pointerId)) return;
    event.preventDefault();
    pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
    const point = geometry();
    const dx = point.x - gesture.x, dy = point.y - gesture.y;
    if (Math.hypot(dx, dy) > 6) moved = true;
    const next = gesture.distance > 0 ?
      Math.max(1, Math.min(5, gesture.scale * point.distance / gesture.distance)) : gesture.scale;
    const rect = stage.getBoundingClientRect();
    x = point.x - rect.left - rect.width / 2 -
      (gesture.x - rect.left - rect.width / 2 - gesture.panX) * next / gesture.scale;
    y = point.y - rect.top - rect.height / 2 -
      (gesture.y - rect.top - rect.height / 2 - gesture.panY) * next / gesture.scale;
    scale = next;
    paint();
  }, { ...options, passive: false });
  const release = event => {
    if (!pointers.has(event.pointerId)) return;
    pointers.delete(event.pointerId);
    if (event.type === 'pointerup' && !moved && !pointers.size) {
      if (lastTap && event.timeStamp - lastTap.time < 300 &&
          Math.hypot(event.clientX - lastTap.x, event.clientY - lastTap.y) < 24) {
        zoom(scale > 1 ? 1 : 2.5, event.clientX, event.clientY);
        lastTap = null;
      } else {
        lastTap = { time: event.timeStamp, x: event.clientX, y: event.clientY };
      }
    } else lastTap = null;
    rebase();
  };
  window.addEventListener('pointerup', release, options);
  window.addEventListener('pointercancel', release, options);
  window.addEventListener('blur', () => { pointers.clear(); gesture = null; lastTap = null; }, options);
  const resize = new ResizeObserver(paint);
  resize.observe(stage);

  return {
    reset,
    open(opener) {
      if (dialog.open || traversing) return;
      returnFocus = document.activeElement;
      selection = returnFocus instanceof window.HTMLTextAreaElement ?
        [returnFocus.selectionStart, returnFocus.selectionEnd, returnFocus.selectionDirection] : null;
      scroller = opener.closest('.chat-messages');
      if (scroller) scrollTop = scroller.scrollTop;
      window.history.pushState({ ...window.history.state, tydeImageViewer: token }, '');
      show();
    },
    destroy() {
      listeners.abort();
      resize.disconnect();
      if (dialog.open) {
        dialog.close();
        restore();
      }
      if (ownsEntry() && !traversing) window.history.back();
    },
  };
}
