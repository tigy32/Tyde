import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const workerSource = readFileSync(
  new URL("../../../mobile-frontend/voice-codec-worker.js", import.meta.url),
  "utf8",
);

test("mobile decoder resampling preserves real-time duration", () => {
  const context = { self: {}, postMessage() {} };
  vm.createContext(context);
  vm.runInContext(workerSource, context);

  const decoded24k = vm.runInContext("new Float32Array(480)", context);
  const rendered48k = context.resample(decoded24k, 24_000, 48_000);
  assert.equal(rendered48k.length / 48_000, decoded24k.length / 24_000);

  const decoded48k = vm.runInContext("new Float32Array(960)", context);
  const unchanged = context.resample(decoded48k, 48_000, 48_000);
  assert.equal(unchanged.length / 48_000, decoded48k.length / 48_000);
});

test("mobile codec failure path is bounded before microphone acquisition", () => {
  const media = readFileSync(
    new URL("../../../mobile-frontend/voice-media.js", import.meta.url),
    "utf8",
  );
  assert.ok(media.indexOf("startWait.promise") < media.indexOf("getUserMedia"));
  const failurePath = media.slice(media.indexOf("} catch (error)"));
  assert.match(failurePath, /await stop\(\)/);
  assert.match(failurePath, /tyde-mobile-voice-error/);
  assert.match(workerSource, /AudioEncoder\.isConfigSupported/);
  assert.match(workerSource, /AudioDecoder\.isConfigSupported/);
});

test("production mobile adapter acquires fresh foreground media and tears down", async () => {
  const mediaSource = readFileSync(
    new URL("../../../mobile-frontend/voice-media.js", import.meta.url),
    "utf8",
  );
  const listeners = new Map();
  const events = [];
  const playbackMessages = [];
  const tracks = [];
  let acquisitions = 0;
  let failCodecStart = false;
  class Worker {
    postMessage(message) {
      queueMicrotask(() => {
        if (message.type === "probe") this.onmessage({ data: { type: "probe-ready" } });
        if (message.type === "start") this.onmessage({ data: failCodecStart ? { type: "error", message: "unsupported opus" } : { type: "start-ready" } });
      });
    }
  }
  class AudioContext {
    constructor() { this.destination = {}; this.audioWorklet = { addModule: async () => {} }; }
    async resume() {}
    createMediaStreamSource() { return { connect() {} }; }
  }
  class AudioWorkletNode {
    constructor(_context, name) { this.port = { postMessage(message) { if (name.includes("playback")) playbackMessages.push(message); }, onmessage: null }; }
    connect() {}
    disconnect() {}
  }
  const window = {
    addEventListener(name, callback) { listeners.set(`window:${name}`, callback); },
    dispatchEvent(event) { events.push(event); },
  };
  const document = {
    hidden: false,
    addEventListener(name, callback) { listeners.set(`document:${name}`, callback); },
  };
  const context = {
    window,
    document,
    navigator: { mediaDevices: { async getUserMedia() {
      acquisitions++;
      const track = { stopped: false, stop() { this.stopped = true; }, getSettings() { return {}; } };
      tracks.push(track);
      return { getTracks: () => [track], getAudioTracks: () => [track] };
    } } },
    Worker,
    AudioContext,
    AudioWorkletNode,
    CustomEvent: class { constructor(type, init) { this.type = type; this.detail = init.detail; } },
    Float32Array,
    performance: { now: () => 1 },
    queueMicrotask,
    setTimeout: (callback, delay) => {
      const timer = setTimeout(callback, delay);
      timer.unref();
      return timer;
    },
    clearTimeout,
  };
  vm.createContext(context);
  vm.runInContext(mediaSource, context);
  const media = window.TydeVoiceMedia;

  await media.prepare();
  assert.equal(acquisitions, 0, "capability preparation must not acquire a microphone");
  await media.start(1);
  assert.equal(acquisitions, 1);
  media.flush();
  const flushMessage = playbackMessages.at(-1);
  assert.notEqual(flushMessage, null);
  assert.equal(typeof flushMessage, "object");
  const flushPrototype = Object.getPrototypeOf(flushMessage);
  assert.ok(
    flushPrototype === null
      || (flushMessage.constructor?.name === "Object"
        && flushPrototype === flushMessage.constructor.prototype),
    "flush must be a plain record",
  );
  const flushKeys = Reflect.ownKeys(flushMessage);
  assert.equal(flushKeys.length, 1, "flush must have exactly one own key");
  assert.equal(flushKeys[0], "type");
  assert.equal(flushMessage.type, "flush");
  await media.stop();
  assert.equal(tracks[0].stopped, true);
  await media.start(2);
  assert.equal(acquisitions, 2, "a new activation must request a fresh source");
  document.hidden = true;
  listeners.get("document:visibilitychange")();
  await Promise.resolve();
  assert.equal(tracks[1].stopped, true, "backgrounding synchronously reaches track.stop");

  failCodecStart = true;
  await assert.rejects(media.start(3), /unsupported opus/);
  assert.equal(acquisitions, 2, "codec failure must occur before microphone acquisition");
  assert.ok(events.some(event => event.type === "tyde-mobile-voice-error"));

  failCodecStart = false;
  await media.start(4);
  listeners.get("window:pagehide")();
  await Promise.resolve();
  assert.equal(tracks[2].stopped, true, "pagehide must release the fresh source");
});
