class TydePlaybackProcessor extends AudioWorkletProcessor {
  constructor() { super(); this.queue=[]; this.offset=0; this.dropped=0; this.port.onmessage=e=>{if(e.data?.type==='flush'){this.queue=[];this.offset=0;return;}this.queue.push(new Float32Array(e.data));while(this.queue.length>10){this.queue.shift();this.offset=0;this.dropped++;}if(this.dropped){this.port.postMessage({type:'drop',packets:this.dropped});this.dropped=0;}}; }
  process(_, outputs) {
    const output=outputs[0]?.[0]; if (!output) return true;
    for(let i=0;i<output.length;i++) { while(this.queue.length && this.offset>=this.queue[0].length){this.queue.shift();this.offset=0;} output[i]=this.queue.length?this.queue[0][this.offset++]:0; }
    return true;
  }
}
registerProcessor('tyde-playback', TydePlaybackProcessor);
