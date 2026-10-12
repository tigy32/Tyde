// Generated from the Rust protocol by tools/generate-dots-codec.py.
const discriminator = "kind";
const variants = {"connection":{"additional":true,"properties":{"paired":{"type":"boolean"}},"required":["paired"]},"request":{"additional":true,"properties":{"body":{"type":"string"},"transport_id":{"type":"string"}},"required":["body","transport_id"]},"response":{"additional":true,"properties":{"body":{"type":"string"},"status":{"maximum":65535,"minimum":0,"type":"integer"},"transport_id":{"type":"string"}},"required":["body","status","transport_id"]}};
const registration = {"additional":true,"properties":{"host_id":{"type":"string"},"host_key":{"type":"string"},"pairing_hash":{"type":"string"}},"required":["host_id","host_key","pairing_hash"]};

function validate(record, definition, excluded = []) {
  if (!record || typeof record !== "object" || Array.isArray(record) || !definition) throw new Error("Invalid connection payload");
  for (const name of definition.required) if (!Object.hasOwn(record, name)) throw new Error("Missing connection field");
  if (!definition.additional && Object.keys(record).some(name => !excluded.includes(name) && !Object.hasOwn(definition.properties, name))) throw new Error("Unknown connection field");
  for (const [name, rule] of Object.entries(definition.properties)) {
    if (!Object.hasOwn(record, name)) continue;
    const value = record[name];
    const valid = rule.type === "integer" ? Number.isInteger(value) : typeof value === rule.type;
    if (!valid || (rule.minimum !== undefined && value < rule.minimum) || (rule.maximum !== undefined && value > rule.maximum)) throw new Error("Invalid connection field");
  }
  return record;
}

function relay(record) { return validate(record, variants[record?.[discriminator]], [discriminator]); }
export const Relay = Object.freeze({
  decode: text => relay(JSON.parse(text)),
  connection: (paired) => JSON.stringify(relay({"kind":"connection", "paired":paired})),
  isConnection: record => record?.[discriminator] === "connection",
  request: (body, transport_id) => JSON.stringify(relay({"kind":"request", "body":body, "transport_id":transport_id})),
  isRequest: record => record?.[discriminator] === "request",
  response: (body, status, transport_id) => JSON.stringify(relay({"kind":"response", "body":body, "status":status, "transport_id":transport_id})),
  isResponse: record => record?.[discriminator] === "response",
});
export const HostRegistration = Object.freeze({
  decode: text => validate(JSON.parse(text), registration),
  encode: (host_id, host_key, pairing_hash) => JSON.stringify(validate({"host_id":host_id, "host_key":host_key, "pairing_hash":pairing_hash}, registration)),
});
