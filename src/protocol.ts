import { GENERATED_CHAT_EVENT_KINDS } from "./generated/protocol_kinds";
import type { ChatEvent } from "./types";

type UnknownRecord = Record<string, unknown>;

function isRecord(value: unknown): value is UnknownRecord {
  return typeof value === "object" && value !== null;
}

function isString(value: unknown): value is string {
  return typeof value === "string";
}

const CORE_EVENT_KIND_SET = new Set<string>(GENERATED_CHAT_EVENT_KINDS);

const CORE_EVENT_REQUIRES_DATA: Record<string, boolean> = Object.fromEntries(
  GENERATED_CHAT_EVENT_KINDS.map((kind) => [
    kind,
    kind !== "ConversationCleared",
  ]),
);

export class ProtocolParseError extends Error {
  payload: unknown;

  constructor(message: string, payload: unknown) {
    super(message);
    this.name = "ProtocolParseError";
    this.payload = payload;
  }
}

export function parseChatEvent(payload: unknown): ChatEvent {
  if (!isRecord(payload) || !isString(payload.kind)) {
    throw new ProtocolParseError("ChatEvent must include string kind", payload);
  }

  if (payload.kind === "SubprocessStderr") {
    if (!isString(payload.data)) {
      throw new ProtocolParseError(
        "SubprocessStderr must include string data",
        payload,
      );
    }
    return { kind: "SubprocessStderr", data: payload.data };
  }

  if (payload.kind === "SubprocessExit") {
    if (!isRecord(payload.data)) {
      throw new ProtocolParseError(
        "SubprocessExit must include object data",
        payload,
      );
    }
    const code = payload.data.exit_code;
    if (!(typeof code === "number" || code === null)) {
      throw new ProtocolParseError(
        "SubprocessExit.exit_code must be number or null",
        payload,
      );
    }
    return { kind: "SubprocessExit", data: { exit_code: code } };
  }

  if (!CORE_EVENT_KIND_SET.has(payload.kind)) {
    throw new ProtocolParseError(
      `Unknown core ChatEvent kind '${payload.kind}'`,
      payload,
    );
  }

  if (CORE_EVENT_REQUIRES_DATA[payload.kind] && !("data" in payload)) {
    throw new ProtocolParseError(
      `Core ChatEvent '${payload.kind}' requires data`,
      payload,
    );
  }

  if (!CORE_EVENT_REQUIRES_DATA[payload.kind]) {
    return { kind: payload.kind } as ChatEvent;
  }

  return {
    kind: payload.kind,
    data: payload.data,
  } as ChatEvent;
}
