// Detailed captures remain opt-in and memory-only. The separate local monitor
// schedules aggregate diagnostics through the typed Rust desktop bridge.
let recorder;

export function createLocalMonitor(env, tick, reset) {
    let timer;
    const now = () => env.performance.now();
    const run = () => {
        tick(now(), env.document.visibilityState === 'visible', env.document.hasFocus());
        timer = env.setTimeout(run, 1000);
    };
    const visibility = () => reset(now());
    env.document.addEventListener('visibilitychange', visibility);
    timer = env.setTimeout(run, 1000);
    return {now, stop() {
        env.clearTimeout(timer);
        env.document.removeEventListener('visibilitychange', visibility);
    }};
}

export function localMonitorNow(monitor) { return monitor.now(); }
export function stopLocalMonitor(monitor) { monitor.stop(); }

function ring(capacity) {
    const rows = new Array(capacity);
    let count = 0;
    return {
        push(row) { rows[count++ % capacity] = row; },
        read() {
            const length = Math.min(count, capacity), start = Math.max(0, count - capacity);
            return Array.from({length}, (_, i) => rows[(start + i) % capacity]);
        },
        overwritten() { return Math.max(0, count - capacity); },
    };
}

export function createRecorder(env) {
    let capture = null, nextId = 1;
    const now = () => env.performance.now();
    const visible = () => env.document.visibilityState === 'visible';
    function begin(operation, detail, bytes, items, waiting) {
        const c = capture;
        if (!c?.running) return 0;
        // Reserve synchronous capacity: many pending remote calls must not hide UI work.
        if ((waiting ? c.waiting : c.stack.length) === 32) { c.droppedActive++; return 0; }
        const id = nextId++;
        const row = {id, parent: c.stack.at(-1) ?? null,
            kind: waiting ? 'async_wait' : 'client_work', operation, detail,
            startMs: now(), bytes, items};
        c.active.set(id, row);
        if (waiting) c.waiting++;
        else c.stack.push(id);
        return id;
    }
    function end(id, outcome) {
        const c = capture, row = c?.active.get(id);
        if (!c?.running || !row) return;
        c.active.delete(id);
        if (row.kind === 'client_work') c.stack.splice(c.stack.indexOf(id), 1);
        else c.waiting--;
        const endMs = now();
        c.records.push({...row, endMs, durationMs: endMs - row.startMs, outcome});
    }
    function setFrame(id, frame) {
        const row = capture?.running && capture.active.get(id);
        if (row) row.detail = frame;
    }
    function setItems(id, items) {
        const row = capture?.running && capture.active.get(id);
        if (row) row.items = items;
    }
    function stop() {
        const c = capture;
        if (!c?.running) return;
        c.running = false;
        c.stoppedMs = now();
        env.clearTimeout(c.timer);
        env.cancelAnimationFrame(c.frame);
        for (const [name, fn] of c.listeners) env.document.removeEventListener(name, fn, true);
        for (const {observer, record} of c.observers) {
            record(observer.takeRecords());
            observer.disconnect();
        }
    }
    function snapshot() {
        const c = capture;
        if (!c) throw new Error('Start a UI diagnostic capture first');
        const endMs = c.running ? now() : c.stoppedMs;
        const active = Array.from(c.active.values()).map(row => ({...row,
            endMs, durationMs: endMs - row.startMs, outcome: 'incomplete'}));
        return {
            schema: 1, running: c.running, startedUnixMs: c.startedUnixMs,
            startMs: c.startMs, endMs, capabilities: {...c.capabilities},
            limits: {records: 1024, gaps: 64, activeClient: 32, activeAsync: 32},
            loss: {recordsOverwritten: c.records.overwritten(), gapsOverwritten: c.gaps.overwritten(),
                activeDropped: c.droppedActive},
            samples: {...c.samples}, records: c.records.read(), active, gaps: c.gaps.read(),
        };
    }
    function start() {
        stop();
        const startMs = now();
        const c = capture = {running: true, startMs, startedUnixMs: env.Date.now(),
            records: ring(1024), gaps: ring(64), active: new Map(), stack: [], waiting: 0,
            droppedActive: 0, samples: {timer: 0, frame: 0, input: 0, hiddenTransitions: 0},
            listeners: [], observers: [], timer: null, frame: null,
            capabilities: {timer: true, animationFrame: true, longtask: 'unsupported',
                longAnimationFrame: 'unsupported'}};
        let timerAt = startMs, frameAt = startMs, foreground = visible();
        const gap = (source, from, to, expectedMs) => {
            c.samples[source]++;
            if (to - from < expectedMs + 100) return;
            const rows = c.records.read();
            const overlapping = rows.filter(row => row.endMs > from && row.startMs < to);
            const work = overlapping.filter(row => row.kind === 'client_work');
            const intervals = work.map(row => [Math.max(from, row.startMs), Math.min(to, row.endMs)])
                .sort((a, b) => a[0] - b[0]);
            let covered = 0, cursor = from;
            for (const [start, end] of intervals) {
                covered += Math.max(0, end - Math.max(cursor, start));
                cursor = Math.max(cursor, end);
            }
            c.gaps.push({source, startMs: from, endMs: to, intervalMs: to - from, expectedMs,
                instrumentedClientUnionMs: covered, unattributedIntervalMs: to - from - covered,
                // Preserve incident context even if later activity rolls the ordinary ring.
                clientWork: work.sort((a, b) => b.durationMs - a.durationMs).slice(0, 12),
                browserWork: overlapping.filter(row => row.kind === 'browser_work').slice(-12),
                recentActivity: rows.filter(row => (row.atMs ?? row.endMs) >= from - 1000).slice(-12),
                pendingAsync: Array.from(c.active.values()).filter(row => row.kind === 'async_wait')});
        };
        const tick = () => {
            if (!c.running) return;
            const t = now();
            if (foreground && visible()) gap('timer', timerAt, t, 100);
            timerAt = t;
            c.timer = env.setTimeout(tick, 100);
        };
        const frame = () => {
            if (!c.running) return;
            const t = now();
            if (foreground && visible()) gap('frame', frameAt, t, 0);
            frameAt = t;
            c.frame = env.requestAnimationFrame(frame);
        };
        const listen = (name, fn) => {
            c.listeners.push([name, fn]);
            env.document.addEventListener(name, fn, true);
        };
        listen('visibilitychange', () => {
            foreground = visible();
            timerAt = frameAt = now();
            c.samples.hiddenTransitions++;
            c.records.push({kind: 'visibility', atMs: now(), visible: foreground});
        });
        for (const name of ['keydown', 'input', 'pointerdown']) listen(name, event => {
            c.samples.input++;
            const t = now(), stamp = event.timeStamp;
            c.records.push({kind: 'interaction', operation: name, atMs: t,
                // Some WebKit versions use an epoch clock for event timestamps.
                deliveryDelayMs: Number.isFinite(stamp) && stamp > 0 && stamp <= t ? t - stamp : null});
        });
        const supported = env.PerformanceObserver?.supportedEntryTypes ?? [];
        for (const [type, key] of [['longtask', 'longtask'], ['long-animation-frame', 'longAnimationFrame']]) {
            if (!supported.includes(type)) continue;
            try {
                const record = entries => {
                    for (const entry of entries) {
                        const row = {kind: 'browser_work', operation: type,
                            startMs: entry.startTime, endMs: entry.startTime + entry.duration,
                            durationMs: entry.duration};
                        c.records.push(row);
                        // Observer delivery can follow the timer/frame callback that
                        // retained the incident. Preserve that late evidence there too.
                        for (const incident of c.gaps.read()) {
                            if (row.endMs > incident.startMs && row.startMs < incident.endMs) {
                                incident.browserWork.push(row);
                                incident.browserWork = incident.browserWork.slice(-12);
                            }
                        }
                    }
                };
                const observer = new env.PerformanceObserver(list => {
                    if (!c.running) return;
                    record(list.getEntries());
                });
                observer.observe({type});
                c.observers.push({observer, record});
                c.capabilities[key] = 'observing';
            } catch {
                c.capabilities[key] = 'registration_failed';
            }
        }
        c.timer = env.setTimeout(tick, 100);
        c.frame = env.requestAnimationFrame(frame);
        return {schema: 1, running: true};
    }
    return {begin, end, setFrame, setItems, start, stop, snapshot, enabled: () => capture?.running === true,
        export: () => JSON.stringify(snapshot()),
        clear() { stop(); capture = null; }};
}

export function installDiagnostics() {
    if (recorder || typeof window === 'undefined') return;
    recorder = createRecorder(window);
    window.__tydeDiagnostics = Object.freeze({start: recorder.start, stop: recorder.stop,
        snapshot: recorder.snapshot, export: recorder.export, clear: recorder.clear});
}

export function diagnosticBegin(operation, detail, bytes, items, waiting) {
    return recorder ? recorder.begin(operation, detail, bytes, items, waiting) : 0;
}
export function diagnosticEnd(id, outcome) { if (id) recorder.end(id, outcome); }
export function diagnosticSetFrame(id, frame) { if (id) recorder.setFrame(id, frame); }

export function diagnosticSetItems(id, items) { if (id) recorder.setItems(id, items); }

// The DOM tests inject a clock and explicit scheduler events, not real delays.
export function installTestDiagnostics(env) {
    if (recorder) recorder.clear();
    recorder = createRecorder(env);
    return recorder;
}

export function diagnosticEnabled() { return recorder?.enabled() === true; }
