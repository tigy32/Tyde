(() => {
    "use strict";
    if (window !== window.top || window.location.origin !== "http://127.0.0.1:18736") {
        throw new Error("Native client loaded outside its bundled origin");
    }
    const post = (endpoint, payload) => {
        const handler = window.webkit.messageHandlers[endpoint];
        if (!handler) throw new Error("Native platform endpoint is unavailable");
        return handler.postMessage(payload);
    };
    Object.defineProperty(window, "__tydeNativeHost", {
        value: Object.freeze({ version: tydeNativeBridgeVersion, post }), writable: false, configurable: false
    });
    Object.defineProperty(window, "__TYDE_MOBILE_SERVICE__", {
        value: Object.freeze({ baseUrl: "https://tycode.dev/api/tyde/mobile/v1", providers: ["apple", "google"] }), writable: false
    });
    const localFetch = window.fetch.bind(window);
    let nextRequestId = 1;
    window.fetch = async (input, options) => {
        const request = new Request(input, options);
        const url = new URL(request.url);
        if (url.origin === tydeNativeAssetOrigin) return localFetch(request);
        if (url.origin === window.location.origin) throw new Error("Bundled resources require the app-owned transport");
        if (url.protocol !== "https:") throw new Error("Native networking requires HTTPS");
        const method = request.method.toLowerCase();
        if (method !== "get" && method !== "post") throw new Error("Native HTTP method is not permitted");
        request.signal.throwIfAborted();
        const body = request.method === "GET" ? null : await request.text();
        request.signal.throwIfAborted();
        if (nextRequestId > 0xffffffff) throw new Error("Native HTTP request sequence exhausted");
        const id = nextRequestId++;
        const pending = post(tydeNativeHttpEndpoint, JSON.stringify({
            id, url: request.url, method, body,
            headers: Object.fromEntries(request.headers.entries())
        }));
        let cancellation;
        const abort = () => {
            cancellation = post(tydeNativeHttpCancelEndpoint, JSON.stringify({ id }));
        };
        request.signal.addEventListener("abort", abort, { once: true });
        try {
            const response = await pending;
            request.signal.throwIfAborted();
            return new Response(response.body, { status: response.status, headers: { "content-type": "application/json" } });
        } catch (error) {
            request.signal.throwIfAborted();
            throw error;
        } finally {
            request.signal.removeEventListener("abort", abort);
            if (cancellation) await cancellation;
        }
    };
})();
