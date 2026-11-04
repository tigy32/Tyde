(async () => {
  'use strict';
  const originKind = location.origin === 'https://tycode.dev' ? 'production'
    : location.origin === 'https://qa-thin-nativebf-fc10ed.tycode.dev' ? 'e266_qa' : null;
  if (!originKind || location.pathname !== '/tyde/' || location.search !== '?native=1' || location.hash ||
      !document.documentElement.hasAttribute('data-tyde-native-bootstrap') ||
      window.__tydeNativeHost?.version !== 6 || window.tydeScannerObservation) {
    throw new Error('Scanner observation requires the original unpaired native entry');
  }
  const sources = {
    'loader.js': '90542cb16d34c91979e89492d61c0e9ecc7a8a2d924e464079e6278ea9c3e926',
    'loader.css': 'e458c101ba2cb1ae4cb6cc373587d394254c6432d8a0113f4493ca0ee865981e',
    'vendor/jsqr.js': '30412062c9e6cc3a182775a6e497e4cc0d0f37765253499183adb1845b40a1d6',
  };
  for (const [path, expected] of Object.entries(sources)) {
    const response = await fetch(path, {cache: 'no-store'});
    if (!response.ok) throw new Error('Scanner observation source fetch failed');
    const hash = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', await response.arrayBuffer())),
      byte => byte.toString(16).padStart(2, '0')).join('');
    if (hash !== expected) throw new Error('Scanner observation source changed; stop');
  }
  const {isPairingScanValue} = await import('/tyde/loader.js');
  const bounds = element => {
    if (!element) return null;
    const r = element.getBoundingClientRect();
    return {x: r.x, y: r.y, width: r.width, height: r.height};
  };
  let next = 0;
  window.tydeScannerObservation = Object.freeze({
    sample(index) {
      if (!Number.isInteger(index) || index !== next || index > 6 || arguments.length !== 1) {
        throw new Error('Only sequential numeric samples 0 through 6 are allowed; never supply QR data');
      }
      const video = document.getElementById('scan-video');
      const pair = document.getElementById('view-pair');
      if (!video || video.hidden || !pair || pair.hidden || !(video.srcObject instanceof MediaStream)) {
        throw new Error('Original live thin scanner is not active; stop');
      }
      const tracks = video.srcObject.getVideoTracks();
      const vv = window.visualViewport;
      const result = {
        probe: 'thin-scanner-observation-v2', originKind, index,
        decoded: false, pairing: false, frameRead: false, failure: null,
        decoderAvailable: typeof window.jsQR === 'function',
        video: {ready: video.readyState, paused: video.paused, width: video.videoWidth, height: video.videoHeight},
        tracks: {count: tracks.length, live: tracks.filter(t => t.readyState === 'live').length,
          muted: tracks.filter(t => t.muted).length},
        viewport: vv ? {scale: vv.scale, top: vv.offsetTop, left: vv.offsetLeft, width: vv.width, height: vv.height} : null,
        document: {width: document.documentElement.clientWidth, height: document.documentElement.clientHeight,
          scrollHeight: document.documentElement.scrollHeight, scrollX, scrollY},
        shell: bounds(document.getElementById('loader-shell')), pair: bounds(pair),
        preview: bounds(video), button: bounds(document.getElementById('scan-button')),
        emptyApp: bounds(document.getElementById('app-root')),
      };
      let stage = 'frame';
      try {
        if (video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA && video.videoWidth && video.videoHeight) {
          const canvas = document.createElement('canvas');
          canvas.width = video.videoWidth; canvas.height = video.videoHeight;
          const context = canvas.getContext('2d', {willReadFrequently: true});
          if (!context) throw new Error();
          context.drawImage(video, 0, 0);
          const frame = context.getImageData(0, 0, canvas.width, canvas.height);
          result.frameRead = true;
          stage = 'decode';
          const decoded = window.jsQR(frame.data, frame.width, frame.height, {inversionAttempts: 'attemptBoth'});
          result.decoded = typeof decoded?.data === 'string';
          result.pairing = result.decoded && isPairingScanValue(decoded.data);
        }
      } catch {
        result.failure = stage;
      }
      next++;
      return JSON.stringify(result);
    },
  });
  return `thin-scanner-observation-v2 ready (${originKind}); sample(0) before gestures, then sample(1)..sample(6)`;
})()
