// Public Tyde mobile managed-service config.
//
// This file is intentionally an external same-origin script so the loader's CSP
// does not need inline JavaScript. It contains only public endpoint metadata;
// session cookies, pairing secrets, broker grants, and Tyggs tokens must never
// be written here.
(() => {
  "use strict";

  window.__TYDE_MOBILE_SERVICE__ = Object.freeze({
    baseUrl: new URL("/api/tyde/mobile/v1", window.location.origin).href,
    provider: "google",
    paywallUrl: "https://tyggs.com/pass",
  });
})();
