// Tiny, dependency-free CBOR reader.
//
// It supports only the definite-length subset that `ciborium` emits when it
// serializes the Tyde pairing payload (`MobilePairingQrPayload`):
//   - unsigned / negative integers (major 0/1)
//   - byte strings and text strings (major 2/3, definite length only)
//   - arrays and maps (major 4/5, definite length only)
//   - tags (major 6, decoded transparently)
//   - simple values + floats (major 7)
//
// Indefinite-length items (additional-info 31) are rejected on purpose: the
// host never produces them, and accepting them would add parsing surface for
// no benefit. This reader is deliberately small so the loader stays tiny and
// auditable — it never `eval`s and never trusts lengths past the input bounds.

export function decodeFirst(bytes) {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  const decoder = new Decoder(view);
  const value = decoder.readItem();
  // We do not require the whole input to be consumed: the loader only needs the
  // top-level map, and trailing bytes (if any) are ignored by the caller. The
  // authoritative re-parse happens later in the real WASM app.
  return value;
}

class Decoder {
  constructor(bytes) {
    this.bytes = bytes;
    this.pos = 0;
  }

  byte() {
    if (this.pos >= this.bytes.length) {
      throw new Error("CBOR: unexpected end of input");
    }
    return this.bytes[this.pos++];
  }

  take(n) {
    if (n < 0 || this.pos + n > this.bytes.length) {
      throw new Error("CBOR: length exceeds input");
    }
    const slice = this.bytes.subarray(this.pos, this.pos + n);
    this.pos += n;
    return slice;
  }

  // Reads the unsigned argument that follows the initial byte for the given
  // additional-info value. Rejects indefinite (31) and reserved (28-30).
  readArg(info) {
    if (info < 24) return info;
    if (info === 24) return this.byte();
    if (info === 25) {
      const b = this.take(2);
      return (b[0] << 8) | b[1];
    }
    if (info === 26) {
      const b = this.take(4);
      return b[0] * 0x1000000 + ((b[1] << 16) | (b[2] << 8) | b[3]);
    }
    if (info === 27) {
      const b = this.take(8);
      const hi = b[0] * 0x1000000 + ((b[1] << 16) | (b[2] << 8) | b[3]);
      const lo = b[4] * 0x1000000 + ((b[5] << 16) | (b[6] << 8) | b[7]);
      // Values above 2^53 lose precision; the pairing payload never carries
      // integers that large, so this is acceptable for the loader's purposes.
      return hi * 0x100000000 + lo;
    }
    throw new Error("CBOR: indefinite or reserved length is not supported");
  }

  readItem() {
    const initial = this.byte();
    const major = initial >> 5;
    const info = initial & 0x1f;

    switch (major) {
      case 0:
        return this.readArg(info);
      case 1:
        return -1 - this.readArg(info);
      case 2:
        return this.take(this.readArg(info)).slice();
      case 3: {
        const len = this.readArg(info);
        return new TextDecoder("utf-8", { fatal: true }).decode(this.take(len));
      }
      case 4: {
        const len = this.readArg(info);
        const arr = [];
        for (let i = 0; i < len; i++) arr.push(this.readItem());
        return arr;
      }
      case 5: {
        const len = this.readArg(info);
        const map = Object.create(null);
        for (let i = 0; i < len; i++) {
          const key = this.readItem();
          const value = this.readItem();
          // Only string keys are meaningful for the pairing payload; non-string
          // keys are stringified so the structure is still walkable.
          map[typeof key === "string" ? key : JSON.stringify(key)] = value;
        }
        return map;
      }
      case 6:
        // Tag: decode and discard the tag number, return the tagged item.
        this.readArg(info);
        return this.readItem();
      case 7:
        return this.readSimple(info);
      default:
        throw new Error("CBOR: unknown major type");
    }
  }

  readSimple(info) {
    switch (info) {
      case 20:
        return false;
      case 21:
        return true;
      case 22:
        return null;
      case 23:
        return undefined;
      case 24:
        return this.byte(); // one-byte simple value
      case 25:
        return readHalfFloat(this.take(2));
      case 26:
        return new DataView(this.take(4).slice().buffer).getFloat32(0, false);
      case 27:
        return new DataView(this.take(8).slice().buffer).getFloat64(0, false);
      default:
        if (info < 20) return info; // small simple value
        throw new Error("CBOR: unsupported simple value or break code");
    }
  }
}

function readHalfFloat(b) {
  const half = (b[0] << 8) | b[1];
  const sign = half & 0x8000 ? -1 : 1;
  const exp = (half >> 10) & 0x1f;
  const frac = half & 0x3ff;
  if (exp === 0) return sign * Math.pow(2, -14) * (frac / 1024);
  if (exp === 31) return frac ? NaN : sign * Infinity;
  return sign * Math.pow(2, exp - 15) * (1 + frac / 1024);
}
