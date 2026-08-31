(() => {
  // Resolve companion assets (worker + worklets) against THIS script's URL,
  // not the page URL: in production the loader page lives at the site root
  // while the bundle's assets live under the versioned directory, so
  // page-relative paths 404 there.
  const SCRIPT_BASE =
    (document.currentScript && document.currentScript.src) ||
    (window.location && window.location.href) ||
    "";
  const assetUrl = (name) =>
    SCRIPT_BASE ? new URL(name, SCRIPT_BASE).toString() : name;

  let context = null;
  let capture = null;
  let playback = null;
  let stream = null;
  let worker = null;
  let generation = 0;
  let samples = [];
  let probeWait = null;
  let startWait = null;

  const dispatch = (name, detail) =>
    window.dispatchEvent(new CustomEvent(name, { detail }));
  const deferred = () => {
    let resolve;
    let reject;
    const promise = new Promise((yes, no) => {
      resolve = yes;
      reject = no;
    });
    return { promise, resolve, reject };
  };

  function installWorker() {
    if (worker) return;
    worker = new Worker(assetUrl("voice-codec-worker.js"));
    worker.onmessage = (event) => {
      if (event.data.type === "probe-ready") {
        probeWait?.resolve(true);
        probeWait = null;
      } else if (event.data.type === "start-ready") {
        startWait?.resolve(true);
        startWait = null;
      } else if (event.data.type === "packet") {
        dispatch("tyde-mobile-voice-packet", event.data);
      } else if (event.data.type === "pcm" && playback) {
        playback.port.postMessage(event.data.pcm, [event.data.pcm.buffer]);
      } else if (event.data.type === "error") {
        const failure = {
          message: String(
            event.data.message || "Browser voice codec failed",
          ).slice(0, 512),
        };
        probeWait?.reject(new Error(failure.message));
        startWait?.reject(new Error(failure.message));
        probeWait = null;
        startWait = null;
        dispatch("tyde-mobile-voice-error", failure);
      }
    };
  }

  async function prepare() {
    context =
      context ||
      new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
    await context.resume();
    await context.audioWorklet.addModule(assetUrl("voice-capture-worklet.js"));
    installWorker();
    probeWait = deferred();
    worker.postMessage({ type: "probe" });
    await Promise.race([
      probeWait.promise,
      new Promise((_, reject) =>
        setTimeout(
          () => reject(new Error("Opus capability probe timed out")),
          3000,
        ),
      ),
    ]);
    return true;
  }

  async function start(options) {
    try {
      await prepare();
      await stopTracks();
      generation = options.generation;
      const inputOnly = options.inputOnly === true;
      startWait = deferred();
      worker.postMessage({ type: "start", generation });
      await Promise.race([
        startWait.promise,
        new Promise((_, reject) =>
          setTimeout(() => reject(new Error("Opus codec start timed out")), 3000),
        ),
      ]);
      const constraints = {
        audio: {
          echoCancellation: !inputOnly,
          noiseSuppression: true,
          autoGainControl: true,
          channelCount: 1,
          sampleRate: 48000,
        },
        video: false,
      };
      stream = await navigator.mediaDevices.getUserMedia(constraints);
      const source = context.createMediaStreamSource(stream);
      capture = new AudioWorkletNode(context, "tyde-capture");
      if (!inputOnly) {
        await context.audioWorklet.addModule(
          assetUrl("voice-playback-worklet.js"),
        );
        playback = new AudioWorkletNode(context, "tyde-playback", {
          outputChannelCount: [1],
        });
        playback.port.onmessage = (event) => {
          if (event.data?.type === "drop") {
            dispatch("tyde-mobile-voice-playback-drop", {
              generation,
              packets: event.data.packets,
            });
          }
        };
        playback.connect(context.destination);
      }
      let timestamp = 0;
      capture.port.onmessage = (event) => {
        const block = new Float32Array(event.data);
        samples.push(...block);
        while (samples.length >= 960) {
          const frame = new Float32Array(samples.splice(0, 960));
          worker.postMessage(
            { type: "pcm", pcm: frame.buffer, timestamp },
            [frame.buffer],
          );
          timestamp += 20000;
        }
      };
      source.connect(capture);
      return stream.getAudioTracks()[0].getSettings();
    } catch (error) {
      await stop();
      dispatch("tyde-mobile-voice-error", {
        message: String(error).slice(0, 512),
      });
      throw error;
    }
  }

  function push({ generation: incoming, opus }) {
    if (incoming !== generation || !worker) return;
    worker.postMessage(
      { type: "opus", opus, timestamp: performance.now() * 1000 },
      [opus.buffer],
    );
  }

  function flush() {
    if (playback) playback.port.postMessage({ type: "flush" });
  }

  async function stopTracks() {
    if (stream) {
      stream.getTracks().forEach((track) => track.stop());
      stream = null;
    }
    if (capture) {
      capture.disconnect();
      capture = null;
    }
    if (playback) {
      playback.disconnect();
      playback = null;
    }
    samples = [];
  }

  async function stop() {
    await stopTracks();
    if (worker) worker.postMessage({ type: "stop" });
    generation = 0;
  }

  const background = () => {
    stop();
    dispatch("tyde-mobile-voice-lifecycle", { reason: "background" });
  };
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) background();
  });
  window.addEventListener("pagehide", background);
  window.TydeVoiceMedia = { prepare, start, push, flush, stop };
})();
