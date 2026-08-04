class TydeCaptureProcessor extends AudioWorkletProcessor {
  process(inputs) {
    const input = inputs[0]?.[0];
    if (input?.length) {
      const copy = new Float32Array(input);
      this.port.postMessage(copy.buffer, [copy.buffer]);
    }
    return true;
  }
}
registerProcessor('tyde-capture', TydeCaptureProcessor);
