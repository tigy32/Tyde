// Bootstrap script for the syntax-highlighting Web Worker.
//
// Why this file is hand-written and unhashed:
//   The main app's wasm-bindgen JS is emitted by Trunk with a
//   content-hash in its filename (`frontend-<hash>.js`). A worker
//   loaded via `new Worker(url, {type:'module'})` is a separate module
//   tree, so it needs to know that hashed URL to `import` the
//   bindings. Trunk doesn't templating into worker assets, so we let
//   the *main thread* discover the hashed URL at runtime (it walks
//   `document.scripts`) and pass both the JS and wasm URLs as query
//   parameters when constructing the Worker.
//
// Wire-up:
//   - main:   `new Worker('/syntax_worker.js?js=<encoded>&wasm=<encoded>',
//                          {type:'module'})`
//   - worker: imports the bindings module, runs its default init
//     (which executes `main()` in worker context — branched in
//     `main.rs` to call `highlight_worker::worker_main()` instead of
//     mounting the Leptos app).

// Buffer any messages that arrive *during* wasm init. Without this,
// messages dispatched before the Rust message handler is registered
// would be silently dropped (the handler in `highlight_worker.rs`
// runs from inside `main()`, which only executes once init resolves).
//
// We replay buffered messages by re-dispatching synthetic
// `MessageEvent`s once init finishes — at which point the Rust
// `set_onmessage` is in place to catch them.
const __pendingBeforeReady = [];
const __preReadyHandler = (e) => __pendingBeforeReady.push(e.data);
self.addEventListener('message', __preReadyHandler);

const params = new URL(self.location.href).searchParams;
const jsUrl = params.get('js');
const wasmUrl = params.get('wasm');

if (!jsUrl || !wasmUrl) {
    throw new Error('syntax_worker.js: missing js= or wasm= query param');
}

const bindings = await import(jsUrl);
await bindings.default({ module_or_path: wasmUrl });

// `init` ran the wasm `main()`, which detected worker context (no
// `window`) and registered the Rust message handler via
// `set_onmessage`. Now drain the pre-init buffer by re-dispatching
// the data through the same channel — Rust's onmessage will pick it
// up.
self.removeEventListener('message', __preReadyHandler);
for (const data of __pendingBeforeReady) {
    self.dispatchEvent(new MessageEvent('message', { data }));
}
