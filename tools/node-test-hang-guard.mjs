import { beforeEach } from "node:test";
import { Worker } from "node:worker_threads";
import { setImmediate } from "node:timers/promises";

if (process.env.NODE_TEST_CONTEXT === "child-v8") {
  const seconds = Number(process.env.TYDE_TEST_HANG_SECONDS);
  if (!Number.isFinite(seconds) || seconds <= 0) throw new Error("Invalid individual-test hung limit");
  // Reporter events can be buffered behind a synchronously blocked test.
  // A separate thread observes completion through shared memory instead.
  const watchdog = new Worker(`
    const { parentPort } = require('node:worker_threads');
    const { writeSync } = require('node:fs');
    parentPort.on('message', ({ state, name, seconds }) => {
      setTimeout(() => {
        if (Atomics.load(new Int32Array(state), 0) === 0) return;
        writeSync(2, 'Hung Node test: ' + name + ' exceeded ' + seconds + 's\\n');
        process.kill(process.pid, 'SIGKILL');
      }, seconds * 1000);
    });
  `, { eval: true });
  await new Promise((resolve, reject) => {
    watchdog.once("online", resolve);
    watchdog.once("error", reject);
  });
  watchdog.unref();
  beforeEach(async context => {
    const state = new Int32Array(new SharedArrayBuffer(4));
    Atomics.store(state, 0, 1);
    context.signal.addEventListener("abort", () => Atomics.store(state, 0, 0), { once: true });
    watchdog.postMessage({ state: state.buffer, name: context.fullName, seconds });
    // Let the runner flush preceding results before this body can block.
    await setImmediate();
  });
}
