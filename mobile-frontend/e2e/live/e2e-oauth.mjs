import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const TEST_OAUTH_PROVIDER = "tyggs_e2e";
const TEST_OAUTH_CALLER_HEADER = "x-tyggs-test-oauth-key";
const FIXTURE_IDS = new Set(
  ["active-pass", "no-pass", "expired-pass", "unverified-contact"].flatMap(
    (state) => [1, 2, 3, 4].map((slot) => `${state}-p${slot}`),
  ),
);

async function loadTestOAuthCallerKey({ region, secretId }) {
  const { stdout } = await execFileAsync(
    "aws",
    [
      "secretsmanager",
      "get-secret-value",
      "--region",
      region,
      "--secret-id",
      secretId,
      "--query",
      "SecretString",
      "--output",
      "text",
    ],
    { maxBuffer: 1024 * 1024 },
  );
  const secret = JSON.parse(stdout);
  if (
    !secret ||
    typeof secret !== "object" ||
    Array.isArray(secret) ||
    Object.keys(secret).length !== 1 ||
    !("TIGYS_TEST_OAUTH_CALLER_KEY" in secret)
  ) {
    throw new Error("The E2E OAuth secret has an unexpected shape");
  }
  const callerKey = secret.TIGYS_TEST_OAUTH_CALLER_KEY;
  if (typeof callerKey !== "string" || Buffer.byteLength(callerKey) < 32) {
    throw new Error("The OAuth secret does not contain a valid E2E caller key");
  }
  return callerKey;
}

async function responseJson(response, operation) {
  if (!response.ok()) {
    throw new Error(`${operation} failed with HTTP ${response.status()}`);
  }
  return response.json();
}

export async function authenticateMobileFixture({
  request,
  fixtureId,
  releaseVersion,
  protocolVersion,
  mobileBaseURL = "https://tycode.dev/api/tyde/mobile/v1",
  accountBaseURL = "https://account.tyggs.com/api/v1",
  region = process.env.AWS_REGION ?? "us-west-2",
  secretId =
    process.env.TYGGS_E2E_OAUTH_SECRET_ID ??
    "tigys-casino/production/e2e-oauth",
}) {
  if (!FIXTURE_IDS.has(fixtureId)) {
    throw new Error("E2E fixture is not allowlisted");
  }
  const callerKey = await loadTestOAuthCallerKey({ region, secretId });
  const startURL = new URL(`${mobileBaseURL}/auth/start`);
  startURL.searchParams.set("provider", TEST_OAUTH_PROVIDER);
  startURL.searchParams.set("return_to", "https://tycode.dev/tyde/");
  const start = await request.get(startURL.href, {
    headers: { [TEST_OAUTH_CALLER_HEADER]: callerKey },
    maxRedirects: 0,
  });
  if (start.status() !== 303) {
    throw new Error(`mobile OAuth start failed with HTTP ${start.status()}`);
  }
  const authorizationURL = start.headers().location;
  const state = authorizationURL
    ? new URL(authorizationURL).searchParams.get("state")
    : null;
  if (!state) {
    throw new Error("mobile OAuth start did not return signed state");
  }

  const callback = await fetch(
    `${accountBaseURL}/auth/oauth/test/callback`,
    {
      method: "POST",
      headers: {
        "content-type": "application/json",
        [TEST_OAUTH_CALLER_HEADER]: callerKey,
      },
      body: JSON.stringify({ state, fixtureId }),
    },
  );
  if (!callback.ok) {
    throw new Error(
      `test OAuth callback failed with HTTP ${callback.status}`,
    );
  }
  const callbackBody = await callback.json();
  if (
    typeof callbackBody.handoffCode !== "string" ||
    !callbackBody.handoffCode
  ) {
    throw new Error("test OAuth callback did not return a handoff code");
  }

  await responseJson(
    await request.post(`${mobileBaseURL}/auth/session`, {
      data: {
        handoff_code: callbackBody.handoffCode,
        client: {
          kind: "mobile_web",
          release_version: releaseVersion,
          protocol_version: protocolVersion,
        },
      },
    }),
    "mobile handoff redeem",
  );
}
