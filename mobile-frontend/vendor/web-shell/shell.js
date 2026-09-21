const owners = new WeakMap();
const textareas = new WeakSet();
const geometryProperties = ["--tws-top", "--tws-left", "--tws-width", "--tws-height", "--tws-clear-top", "--tws-clear-bottom"];
function preserveStyle(element, properties) {
    const previous = properties.map(name => [name, element.style.getPropertyValue(name), element.style.getPropertyPriority(name)]);
    return () => {
        for (const [name, value, priority] of previous) {
            if (value)
                element.style.setProperty(name, value, priority);
            else
                element.style.removeProperty(name);
        }
    };
}
function preserveAttribute(element, name) {
    const previous = element.getAttribute(name);
    return () => {
        if (previous === null)
            element.removeAttribute(name);
        else
            element.setAttribute(name, previous);
    };
}
function editable(element) {
    if (!element || !("focus" in element))
        return false;
    if (element.matches("textarea:not(:disabled):not([readonly]), [contenteditable]:not([contenteditable=false])"))
        return true;
    return element.matches('input:not(:disabled):not([readonly]):not([type=button]):not([type=submit]):not([type=reset]):not([type=checkbox]):not([type=radio]):not([type=range]):not([type=color]):not([type=file]):not([type=hidden])');
}
function required(root, selector) {
    const element = root.querySelector(selector);
    if (!element)
        throw new Error(`Web shell requires ${selector}`);
    return element;
}
export function attachShell(root, options = {}) {
    const document = root.ownerDocument;
    const view = document.defaultView;
    if (!view || !root.isConnected)
        throw new Error("Attach a connected shell element");
    const window = view;
    if (owners.has(document))
        throw new Error("Only one web shell may own a document");
    const header = options.regions?.header ?? required(root, ":scope > .tws-header");
    const content = options.regions?.content ?? required(root, ":scope > .tws-content");
    const bottom = options.regions?.bottom ?? required(root, ":scope > .tws-bottom");
    const flow = options.regions?.flow ?? required(content, ":scope > .tws-flow");
    if (![header, content, bottom, flow].every(element => root.contains(element)))
        throw new Error("Shell regions must belong to its root");
    const html = document.documentElement;
    const restore = [preserveStyle(root, geometryProperties), preserveAttribute(root, "data-tws-keyboard"), preserveAttribute(root, "data-tws-title-hidden"), preserveAttribute(html, "data-tws-document")];
    const viewport = window.visualViewport;
    const keyboard = window.navigator.virtualKeyboard;
    if (keyboard) {
        const previous = keyboard.overlaysContent;
        keyboard.overlaysContent = false;
        restore.push(() => { keyboard.overlaysContent = previous; });
    }
    let browserChrome = Math.max(0, window.outerHeight - html.clientHeight);
    const coarsePointer = window.matchMedia("(any-pointer: coarse)");
    const following = options.followEnd === true;
    let pinned = options.initialScroll === "end";
    let stopped = false;
    let dirty = true;
    let frame = 0;
    let settleUntil = 0;
    let state = { top: 0, left: 0, width: html.clientWidth, height: html.clientHeight, keyboard: false, scale: 1 };
    const written = new Map();
    const registrations = [];
    const write = (name, value) => {
        const text = `${Math.round(value * 1000) / 1000}px`;
        if (written.get(name) === text)
            return;
        root.style.setProperty(name, text);
        written.set(name, text);
    };
    const atEnd = () => content.scrollHeight - content.clientHeight - content.scrollTop <= 48;
    const toEnd = () => { content.scrollTop = content.scrollHeight; };
    const queue = () => {
        if (!stopped && !document.hidden && frame === 0)
            frame = window.requestAnimationFrame(tick);
    };
    const invalidate = () => { dirty = true; queue(); };
    const settle = () => { settleUntil = window.performance.now() + 900; invalidate(); };
    const read = () => {
        const scale = viewport?.scale ?? 1;
        if (Math.abs(scale - 1) > 0.01)
            return null;
        const layoutHeight = html.clientHeight || window.innerHeight;
        const layoutWidth = html.clientWidth || window.innerWidth;
        const top = Math.max(0, viewport?.offsetTop ?? 0);
        const left = Math.max(0, viewport?.offsetLeft ?? 0);
        const width = Math.max(1, Math.min(viewport?.width ?? layoutWidth, layoutWidth - left));
        const focused = editable(document.activeElement);
        if (!focused && !state.keyboard)
            browserChrome = Math.max(0, window.outerHeight - layoutHeight);
        const referenceHeight = keyboard && coarsePointer.matches ? Math.max(layoutHeight, window.outerHeight - browserChrome) : layoutHeight;
        const height = Math.max(1, Math.min(viewport?.height ?? layoutHeight, layoutHeight - top));
        return { top, left, width, height, scale,
            keyboard: (focused || state.keyboard) && referenceHeight - (viewport?.height ?? layoutHeight) > 80 };
    };
    const reveal = () => {
        const active = document.activeElement;
        if (!editable(active) || !content.contains(active))
            return;
        const bounds = active.getBoundingClientRect();
        const shell = root.getBoundingClientRect();
        const upper = header.hidden ? shell.top + 8 : Math.max(shell.top + 8, header.getBoundingClientRect().bottom + 8);
        const lower = bottom.hidden ? shell.bottom - 8 : Math.min(shell.bottom - 8, bottom.getBoundingClientRect().top - 8);
        if (bounds.top < upper)
            content.scrollTop -= upper - bounds.top;
        else if (bounds.bottom > lower)
            content.scrollTop += Math.min(bounds.bottom - lower, bounds.top - upper);
    };
    const apply = () => {
        const next = read();
        if (!next)
            return;
        const changed = next.top !== state.top || next.left !== state.left || next.width !== state.width || next.height !== state.height || next.keyboard !== state.keyboard;
        if (!dirty && !changed)
            return;
        dirty = false;
        state = next;
        write("--tws-top", next.top);
        write("--tws-left", next.left);
        write("--tws-width", next.width);
        write("--tws-height", next.height);
        root.toggleAttribute("data-tws-keyboard", next.keyboard);
        const shell = root.getBoundingClientRect();
        const head = header.getBoundingClientRect();
        const composerHeight = bottom.querySelector(":scope > .tws-composer")?.getBoundingClientRect().height ?? 0;
        const bottomGap = parseFloat(window.getComputedStyle(bottom).marginBottom) || 0;
        const titleHidden = head.height > 0 && composerHeight > 0 && !bottom.hidden && head.bottom - shell.top + composerHeight + bottomGap + 4 > next.height;
        root.toggleAttribute("data-tws-title-hidden", titleHidden);
        const foot = bottom.getBoundingClientRect();
        const pane = content.getBoundingClientRect();
        write("--tws-clear-top", titleHidden ? 0 : head.height > 0 ? Math.max(0, head.bottom - pane.top) + 12 : 12);
        write("--tws-clear-bottom", foot.height > 0 ? Math.max(0, pane.bottom - foot.top) + 12 : 12);
        if (following && pinned)
            toEnd();
        reveal();
    };
    function tick() {
        frame = 0;
        if (stopped || document.hidden)
            return;
        apply();
        if (editable(document.activeElement) || window.performance.now() < settleUntil)
            queue();
    }
    const listen = (target, name, callback, capture = false) => {
        target.addEventListener(name, callback, capture);
        registrations.push([target, name, callback, capture]);
    };
    const pointer = (event) => {
        const pointer = event;
        if (!pointer.isTrusted || !pointer.isPrimary || pointer.button !== 0)
            return;
        const element = event.target?.closest("input, textarea, [contenteditable]") ?? null;
        if (editable(element) && element !== document.activeElement)
            element.focus({ preventScroll: true });
    };
    const visibility = () => {
        if (document.hidden && frame !== 0) {
            window.cancelAnimationFrame(frame);
            frame = 0;
        }
        else
            settle();
    };
    const scroll = () => { pinned = atEnd(); };
    const observer = new ResizeObserver(invalidate);
    for (const element of [header, bottom, content, flow])
        observer.observe(element);
    for (const name of ["resize", "orientationchange", "pageshow"])
        listen(window, name, settle);
    if (viewport)
        for (const name of ["resize", "scroll"])
            listen(viewport, name, settle);
    for (const name of ["focusin", "focusout"])
        listen(document, name, settle);
    listen(document, "visibilitychange", visibility);
    listen(root, "pointerdown", pointer, true);
    listen(content, "scroll", scroll);
    owners.set(document, root);
    html.setAttribute("data-tws-document", "");
    apply();
    if (options.initialScroll === "end")
        toEnd();
    else if (options.initialScroll === "start")
        content.scrollTop = 0;
    pinned = atEnd();
    queue();
    return {
        refresh: invalidate,
        scrollToEnd() { if (!stopped) {
            pinned = true;
            toEnd();
        } },
        getState: () => ({ ...state, scale: viewport?.scale ?? 1 }),
        destroy() {
            if (stopped)
                return;
            stopped = true;
            if (frame !== 0)
                window.cancelAnimationFrame(frame);
            observer.disconnect();
            for (const [target, name, callback, capture] of registrations)
                target.removeEventListener(name, callback, capture);
            for (const reset of restore.reverse())
                reset();
            owners.delete(document);
        },
    };
}
export function attachTextarea(element) {
    if (textareas.has(element))
        throw new Error("Textarea already has a sizing owner");
    const window = element.ownerDocument.defaultView;
    if (!window)
        throw new Error("Textarea has no window");
    const restore = preserveStyle(element, ["height", "overflow-y"]);
    let stopped = false;
    let frame = 0;
    let resizing = false;
    const resize = () => {
        if (stopped || resizing || element.getClientRects().length === 0)
            return;
        resizing = true;
        const scroll = element.scrollTop;
        const style = window.getComputedStyle(element);
        const border = parseFloat(style.borderTopWidth) + parseFloat(style.borderBottomWidth);
        const min = parseFloat(style.minHeight) || 0;
        const max = parseFloat(style.maxHeight) || Infinity;
        element.style.height = "0px";
        const natural = element.scrollHeight + border;
        element.style.height = `${Math.max(min, Math.min(natural, max))}px`;
        element.style.overflowY = natural > max ? "auto" : "hidden";
        element.scrollTop = scroll;
        resizing = false;
    };
    const queue = () => {
        if (!stopped && frame === 0)
            frame = window.requestAnimationFrame(() => { frame = 0; resize(); });
    };
    const observer = new ResizeObserver(queue);
    observer.observe(element);
    const container = element.closest(".tws-shell");
    if (container)
        observer.observe(container);
    element.addEventListener("input", resize);
    window.addEventListener("resize", queue);
    window.visualViewport?.addEventListener("resize", queue);
    element.ownerDocument.fonts.addEventListener("loadingdone", queue);
    textareas.add(element);
    resize();
    return () => {
        if (stopped)
            return;
        stopped = true;
        observer.disconnect();
        if (frame !== 0)
            window.cancelAnimationFrame(frame);
        element.removeEventListener("input", resize);
        window.removeEventListener("resize", queue);
        window.visualViewport?.removeEventListener("resize", queue);
        element.ownerDocument.fonts.removeEventListener("loadingdone", queue);
        textareas.delete(element);
        restore();
    };
}
