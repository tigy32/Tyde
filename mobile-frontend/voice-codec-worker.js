let encoder=null, decoder=null, generation=0, seq=0;
const encoderConfig={codec:'opus',sampleRate:48000,numberOfChannels:1,bitrate:24000};
const decoderConfig={codec:'opus',sampleRate:24000,numberOfChannels:1};
async function reset(){
  if(encoder){await encoder.flush().catch(()=>{});encoder.close();encoder=null;}
  if(decoder){decoder.close();decoder=null;}
}
async function supported(){
  if(!self.AudioEncoder||!self.AudioDecoder)return false;
  const [encode,decode]=await Promise.all([AudioEncoder.isConfigSupported(encoderConfig),AudioDecoder.isConfigSupported(decoderConfig)]);
  return encode.supported===true&&decode.supported===true;
}
function resample(source,fromRate,toRate){
  if(fromRate===toRate)return source;
  const length=Math.max(1,Math.round(source.length*toRate/fromRate));
  const output=new Float32Array(length);
  for(let i=0;i<length;i++){const position=i*fromRate/toRate;const left=Math.min(source.length-1,Math.floor(position));const right=Math.min(source.length-1,left+1);const fraction=position-left;output[i]=source[left]*(1-fraction)+source[right]*fraction;}
  return output;
}
self.onmessage=async event=>{
  const m=event.data;
  try{
    if(m.type==='probe'){
      if(!await supported())throw new Error('WebCodecs Opus encode/decode is unavailable');
      postMessage({type:'probe-ready'});
    }else if(m.type==='start'){
      await reset();
      if(!await supported())throw new Error('WebCodecs Opus encode/decode is unavailable');
      generation=m.generation;seq=0;
      encoder=new AudioEncoder({output:chunk=>{const data=new Uint8Array(chunk.byteLength);chunk.copyTo(data);postMessage({type:'packet',generation,seq:seq++,opus:data},[data.buffer]);},error:error=>postMessage({type:'error',message:String(error)})});
      encoder.configure(encoderConfig);
      decoder=new AudioDecoder({output:frame=>{const source=new Float32Array(frame.numberOfFrames);frame.copyTo(source,{planeIndex:0});const data=resample(source,frame.sampleRate,48000);postMessage({type:'pcm',generation,pcm:data},[data.buffer]);frame.close();},error:error=>postMessage({type:'error',message:String(error)})});
      decoder.configure(decoderConfig);
      postMessage({type:'start-ready',generation});
    }else if(m.type==='pcm'&&encoder){
      const samples=new Float32Array(m.pcm);const frame=new AudioData({format:'f32-planar',sampleRate:48000,numberOfFrames:samples.length,numberOfChannels:1,timestamp:m.timestamp,data:samples});encoder.encode(frame);frame.close();
    }else if(m.type==='opus'&&decoder){decoder.decode(new EncodedAudioChunk({type:'key',timestamp:m.timestamp||0,data:m.opus}));}
    else if(m.type==='stop'){await reset();generation=0;seq=0;}
  }catch(error){await reset();generation=0;postMessage({type:'error',message:String(error)});}
};
