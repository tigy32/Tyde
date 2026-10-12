#!/usr/bin/env python3
"""Generate the service wire codec from schemas exported by the Rust host."""
import argparse
import json
import pathlib
import sys


def resolve(schema, root):
    if "$ref" in schema:
        prefix = "#/$defs/"
        reference = schema["$ref"]
        if not reference.startswith(prefix):
            raise ValueError("Unsupported connection-schema reference")
        return resolve(root["$defs"][reference[len(prefix):]], root)
    return schema


def record(schema, root, excluded=()):
    properties = {}
    for name, value in sorted(schema["properties"].items()):
        if name in excluded:
            continue
        value = resolve(value, root)
        if value.get("type") not in {"boolean", "string", "integer"}:
            raise ValueError("Unsupported connection-schema field type")
        properties[name] = {"type": value["type"]}
        for key in ("minimum", "maximum"):
            if key in value:
                properties[name][key] = int(value[key])
        unsupported = set(value) - {"type", "minimum", "maximum", "format", "title", "description"}
        if unsupported:
            raise ValueError("Unsupported connection-schema field constraint")
    return {"properties": properties,
            "required": sorted(set(schema["required"]) - set(excluded)),
            "additional": schema.get("additionalProperties", True)}


def generate(schemas):
    relay = schemas["relay"]
    variants = relay["oneOf"]
    constants = [dict((name, value["const"]) for name, value in item["properties"].items() if "const" in value) for item in variants]
    tags = set(constants[0]).intersection(*(set(value) for value in constants[1:]))
    if len(tags) != 1:
        raise ValueError("Connection enum must have one explicit discriminator")
    discriminator = tags.pop()
    definitions = {value[discriminator]: record(item, relay, (discriminator,)) for item, value in zip(variants, constants)}
    registration = record(schemas["host_registration"], schemas["host_registration"])
    compact = lambda value: json.dumps(value, sort_keys=True, separators=(",", ":"))
    output = ["// Generated from the Rust protocol by tools/generate-dots-codec.py.",
              f"const discriminator = {compact(discriminator)};",
              f"const variants = {compact(definitions)};",
              f"const registration = {compact(registration)};", ""]
    output.append('''function validate(record, definition, excluded = []) {
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
  decode: text => relay(JSON.parse(text)),''')
    for tag, definition in sorted(definitions.items()):
        fields = sorted(definition["properties"])
        if not all(name.isidentifier() for name in [tag, *fields]):
            raise ValueError("Connection bindings require identifier names")
        entries = [f"{compact(discriminator)}:{compact(tag)}", *(f"{compact(name)}:{name}" for name in fields)]
        output.append(f"  {tag}: ({', '.join(fields)}) => JSON.stringify(relay({{{', '.join(entries)}}})),")
        output.append(f"  is{tag[0].upper() + tag[1:]}: record => record?.[discriminator] === {compact(tag)},")
    fields = sorted(registration["properties"])
    entries = ", ".join(f"{compact(name)}:{name}" for name in fields)
    output.extend(["});", "export const HostRegistration = Object.freeze({",
                   "  decode: text => validate(JSON.parse(text), registration),",
                   f"  encode: ({', '.join(fields)}) => JSON.stringify(validate({{{entries}}}, registration)),",
                   "});", ""])
    return "\n".join(output)


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    args = parser.parse_args()
    generated = generate(json.load(sys.stdin))
    destination = pathlib.Path(__file__).resolve().parent.parent / "services/chatgpt-dots/codec.mjs"
    if args.write:
        destination.write_text(generated)
    elif destination.read_text() != generated:
        raise SystemExit("Dots wire bindings are stale; regenerate from tyde-server dots-schema")


if __name__ == "__main__":
    main()
