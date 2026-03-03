import { spawn, type ChildProcess } from "child_process";

let viteServer: ChildProcess;

export const config = {
  runner: "local",
  specs: ["./tests/e2e/**/*.test.ts"],
  maxInstances: 1,

  capabilities: [
    {
      browserName: "chrome",
      "goog:chromeOptions": {
        args: ["--headless", "--no-sandbox", "--disable-gpu", "--window-size=1280,800"],
      },
    },
  ],

  baseUrl: "http://localhost:1420",
  logLevel: "warn",
  waitforTimeout: 10000,
  connectionRetryTimeout: 120000,
  connectionRetryCount: 3,

  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 60000,
  },

  onPrepare() {
    viteServer = spawn("npx", ["vite", "dev", "--config", "vite.config.test.ts"], {
      stdio: [null, process.stdout, process.stderr],
      cwd: process.cwd(),
    });
    return new Promise<void>((resolve) => setTimeout(resolve, 5000));
  },

  onComplete() {
    viteServer.kill();
  },

  before() {
    return browser.url("/");
  },
};
