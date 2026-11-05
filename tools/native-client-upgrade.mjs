// Canonical server sim: immutable first shell + actual source-built clients.
import {readFile, writeFile, mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {resolve as resolvePath, sep, extname} from 'node:path';
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {createServer as httpsServer} from 'node:https';
import {createServer as httpServer, request as httpRequest} from 'node:http';
import {connect} from 'node:net';
import {once} from 'node:events';
import {createInterface} from 'node:readline';
import {runInNewContext} from 'node:vm';
import {chromium} from '@playwright/test';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const desktopStyles = await readFile('frontend/styles.css','utf8');
const decoderSource = await readFile('web/loader/decoder/jsqr-5f0b827026fedad24be5147cc65e8217e06a643b3967206e91c9d1332e1b3101.js','utf8');
const decoderEntry = 'return __webpack_require__(__webpack_require__.s = 3);';
if(decoderSource.split(decoderEntry).length!==2)throw Error('QR diagnostic source identity changed');
const decoderScope={module:{exports:{}}};
runInNewContext(decoderSource.replace(decoderEntry,'globalThis.__thinQrModules=__webpack_require__;'+decoderEntry)
  .replace('function readVersion(matrix) {','exports.__thinReadVersion=readVersion; function readVersion(matrix) {'),decoderScope);
const qrModules=decoderScope.__thinQrModules;
const diagnoseQrPipeline=({data,width,height,bits,dimension,svg_x,svg_y,svg_width,svg_height},uri)=>{
  const grid=dimension-4;
  const expected=qrModules(0).BitMatrix.createEmpty(grid,grid);
  for(let y=0;y<grid;y++)for(let x=0;x<grid;x++)expected.set(x,y,bits[(y+2)*dimension+x+2]);
  const reference=qrModules(5).decode(expected);
  const {binarized}=qrModules(4).binarize(Uint8ClampedArray.from(data),width,height,false);
  let binaryMismatch=0;
  for(let y=0;y<dimension;y++)for(let x=0;x<dimension;x++){
    if(binarized.get(Math.floor(svg_x+(x+.5)*svg_width/dimension),Math.floor(svg_y+(y+.5)*svg_height/dimension))!==!!bits[y*dimension+x])binaryMismatch++;
  }
  const locations=qrModules(12).locate(binarized)||[];
  return {reference_matrix_decoded:!!reference,reference_matrix_exact:reference?.text===uri,binarized_module_mismatch:binaryMismatch,
    locations:locations.map(location=>{
      const {matrix}=qrModules(11).extract(binarized,location);
      let mismatch=0;
      if(matrix.width===grid)for(let y=0;y<grid;y++)for(let x=0;x<grid;x++)if(matrix.get(x,y)!==!!bits[(y+2)*dimension+x+2])mismatch++;
      const version=qrModules(5).__thinReadVersion(matrix);
      let refinement=null;
      if(version){
        const refinedDimension=version.versionNumber*4+17;
        const refined=qrModules(11).extract(binarized,{...location,dimension:refinedDimension}).matrix;
        let difference=0;
        if(refined.width===grid)for(let y=0;y<grid;y++)for(let x=0;x<grid;x++)if(refined.get(x,y)!==!!bits[(y+2)*dimension+x+2])difference++;
        const result=qrModules(5).decode(refined);
        refinement={encoded_version:version.versionNumber,dimension:refinedDimension,module_mismatch:refined.width===grid?difference:null,decoded:!!result,exact:result?.text===uri};
      }
      const decoded=qrModules(5).decode(matrix);
      return {dimension:location.dimension,extracted_dimension:matrix.width,same_dimension:matrix.width===grid,
        module_mismatch:matrix.width===grid?mismatch:null,decoded:!!decoded,exact:decoded?.text===uri,refinement};
    })};
};
const temporary = await mkdtemp(resolvePath(tmpdir(), 'tyde-native-client-'));
const sockets = new Set();
const transports = new Set();
const requests = new Set();
let browser, publisher, proxy, page, evidence, closing=false, stage = 'shell custody';
try {
  const custody = JSON.parse(await readFile('tests/fixtures/native-bootstrap-integrated-v1.json','utf8'));
  const archive = resolvePath('tests/fixtures/native-bootstrap-integrated-v1.tar.gz');
  if (sha(await readFile(archive)) !== custody.archive_sha256) throw Error();
  execFileSync('tar',['-xzf',archive,'-C',temporary],{stdio:'ignore'});
  const shell = resolvePath(temporary,'Shell');
  const metadata = JSON.parse(await readFile(resolvePath(shell,'Web/native-manifest.json'),'utf8'));
  const bridgeScript = await readFile(resolvePath(shell,'native-host.js'),'utf8');
  const contractBytes=await readFile(resolvePath(shell,'NativeBridgeContract.json'));
  if(sha(contractBytes)!==custody.native_contract_sha256)throw Error('Installed native contract custody failed');
  const contract=JSON.parse(contractBytes);
  if(contract.bridge_version!==metadata.bridge_version||contract.bootstrap_version!==metadata.bootstrap_version)throw Error();
  const accepts=(value,schema)=>{
    if(schema.anyOf)return schema.anyOf.some(option=>accepts(value,option));
    if(schema.enum&&!schema.enum.includes(value))return false;
    switch(schema.type){
      case 'null':return value===null;
      case 'string':return typeof value==='string';
      case 'boolean':return typeof value==='boolean';
      case 'integer':return Number.isInteger(value)&&value>=schema.minimum&&value<=schema.maximum;
      case 'array':return Array.isArray(value)&&value.every(item=>accepts(item,schema.items));
      case 'object':return value!==null&&typeof value==='object'&&!Array.isArray(value)&&
        (schema.required||[]).every(key=>Object.hasOwn(value,key))&&
        Object.entries(schema.properties||{}).every(([key,shape])=>!Object.hasOwn(value,key)||accepts(value[key],shape))&&
        (typeof schema.additionalProperties!=='object'||Object.values(value).every(item=>accepts(item,schema.additionalProperties)));
      default:throw Error('Unsupported frozen native contract schema');
    }
  };
  const requireRequest=(endpoint,body)=>{
    if(!contract.requests[endpoint]||!accepts(body,contract.requests[endpoint]))throw Error('The unchanged installed native ABI rejected a client request');
  };

  const swift = await readFile(resolvePath(shell,'NativeBridge.swift'),'utf8');
  if (sha(bridgeScript)!==metadata.bridge_sha256 || metadata.bridge_version!==6 || metadata.bootstrap_version!==1 ||
      metadata.release!==custody.shell_release || /nativeProtocolVersion|FrameKind|HostBootstrap/.test(swift)) throw Error();
  const lines = createInterface({input:process.stdin});
  const commands = lines[Symbol.asyncIterator]();
  const receive = async () => JSON.parse((await commands.next()).value);
  const emit = event => process.stdout.write(JSON.stringify(event)+'\n');
  const initial = await receive();
  evidence=initial.evidence;
  const bundles = [initial.current_bundle, initial.baseline_bundle].filter(Boolean);
  const manifests = await Promise.all(bundles.map(async root=>JSON.parse(await readFile(resolvePath(root,'manifest.json'),'utf8'))));
  const published = {...manifests[0],versions:Object.assign({},...manifests.map(m=>m.versions))};
  delete published.servedClientRelease;
  published.minSupported = manifests.at(-1).minSupported;
  const entryFor = root => manifests[bundles.indexOf(root)].servedClientRelease;
  const targets = new Map();
  let pairRequests=0, documents=0, scope=null, presentation=null, publisherFault=null;
  let releasePublisher=null, initialPublicationHeld=false;
  let holdPairingManifest=false, pairingManifestArrived=null, releasePairingManifest=null, pairingOutcome=null, pairingCancellationObserved=null;
  let failNextHostPublication=false, holdNextHostPublication=false, releaseHostPublication=null;
  const secrets = new Map();
  const pendingHttp = new Map();
  let offlineCount=0, offlineObserved=null;
  let rejectNextUpgrade=false, droppedUpgrade=null, rediscovered=null, networkDrops=0;
  const mainDiscovery={requests:0,responses:0,device_requests:0,last_status:null};
  execFileSync('openssl',['req','-x509','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-nodes','-keyout',resolvePath(temporary,'key.pem'),'-out',resolvePath(temporary,'cert.pem'),'-days','1','-subj','/CN=tycode.dev','-addext','subjectAltName=DNS:tycode.dev'],{stdio:'ignore'});
  const publicKey=execFileSync('openssl',['x509','-in',resolvePath(temporary,'cert.pem'),'-pubkey','-noout']);
  const spki=execFileSync('openssl',['pkey','-pubin','-outform','DER'],{input:publicKey});
  const certificatePin=createHash('sha256').update(spki).digest('base64');
  const mime = {'.html':'text/html','.js':'application/javascript','.mjs':'application/javascript','.css':'text/css','.wasm':'application/wasm','.json':'application/json','.svg':'image/svg+xml','.png':'image/png','.woff2':'font/woff2'};
  publisher = httpsServer({key:await readFile(resolvePath(temporary,'key.pem')),cert:await readFile(resolvePath(temporary,'cert.pem'))},async(req,res)=>{
    try {
      const host=req.headers.host?.split(':')[0];
      if(host!=='tycode.dev') {
        const address=targets.get(host); if(!address)throw Error();
        const outgoing=httpRequest(`http://${address}${req.url}`,{method:req.method,headers:req.headers},incoming=>{
          res.writeHead(incoming.statusCode,incoming.headers);incoming.pipe(res);
        });requests.add(outgoing);outgoing.on('close',()=>requests.delete(outgoing));outgoing.on('error',()=>res.destroy());req.pipe(outgoing);return;
      }
      const path=new URL(req.url,'https://tycode.dev').pathname;
      if(path==='/tyde/manifest.json'){
        if(holdPairingManifest&&req.headers['x-tyde-native-publication-sim']!=='1'&&req.headers['x-tyde-native-pairing-test']==='1'){
          holdPairingManifest=false;
          const released=new Promise(resolve=>releasePairingManifest=resolve);
          pairingManifestArrived({pairing:req.headers['x-tyde-native-pairing-test']==='1'});await released;
        }
        if(req.headers['x-tyde-native-publication-sim']==='1'){
          if(!initialPublicationHeld){
            initialPublicationHeld=true;
            const released=new Promise(resolve=>releasePublisher=resolve);
            emit({event:'publisher-held'});await released;
          }
          if(holdNextHostPublication){
            holdNextHostPublication=false;
            const released=new Promise(resolve=>releaseHostPublication=resolve);
            emit({event:'managed-publication-held'});await released;
          }
          if(failNextHostPublication){failNextHostPublication=false;res.writeHead(503).end();emit({event:'publication-host-failed'});return;}
        }
        if(publisherFault==='unavailable'){res.writeHead(503).end();return;}
        const manifest=structuredClone(published);
        if(publisherFault==='blocked')manifest.blocked=[...Object.keys(manifest.versions)];
        if(publisherFault==='abi')for(const entry of Object.values(manifest.versions))entry.nativeBridgeVersions=[];
        res.writeHead(200,{'content-type':'application/json','cache-control':'no-store'}).end(JSON.stringify(manifest));return;
      }
      let root=resolvePath(initial.current_bundle),relative=path==='/tyde/'?'index.html':path.slice('/tyde/'.length);
      for(const bundle of bundles){if(path.startsWith(`/tyde/v${entryFor(bundle)}/`)){root=resolvePath(bundle);break;}}
      const file=resolvePath(root,relative);
      if(!path.startsWith('/tyde/')||!file.startsWith(root+sep))throw Error();
      let bytes=await readFile(file);
      if(publisherFault==='integrity'&&extname(file)==='.wasm')bytes=Buffer.concat([bytes,Buffer.from([1])]);
      res.writeHead(200,{'content-type':mime[extname(file)]||'application/octet-stream','cache-control':'no-store'}).end(bytes);
    }catch{res.writeHead(404).end();}
  });
  publisher.on('connection',socket=>{transports.add(socket);socket.on('close',()=>transports.delete(socket));});
  publisher.on('tlsClientError',()=>emit({event:'publisher-tls-failed'}));
  publisher.on('upgrade',(req,socket,head)=>{
    if(req.headers.host?.split(':')[0]==='native-host.test'&&rejectNextUpgrade){
      rejectNextUpgrade=false;networkDrops++;socket.destroy();droppedUpgrade?.();return;
    }
    const address=targets.get(req.headers.host?.split(':')[0]);if(!address){socket.destroy();return;}
    sockets.add(socket);socket.on('close',()=>sockets.delete(socket));
    const [host,port]=address.split(':');
    const upstream=connect({host,port:Number(port)},()=>{
      upstream.write(`${req.method} ${req.url} HTTP/${req.httpVersion}\r\n`+req.rawHeaders.reduce((s,x,i)=>s+x+(i%2?'\r\n':': '),'')+'\r\n');
      if(head.length)upstream.write(head);socket.pipe(upstream);upstream.pipe(socket);
    });upstream.on('error',()=>socket.destroy());socket.on('error',()=>upstream.destroy());socket.on('close',()=>upstream.destroy());
  });
  publisher.listen(0,'127.0.0.1');await once(publisher,'listening');
  proxy=httpServer();proxy.on('connect',(_req,socket,head)=>{
    const upstream=connect({host:'127.0.0.1',port:publisher.address().port},()=>{
      socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');if(head.length)upstream.write(head);socket.pipe(upstream);upstream.pipe(socket);
    });upstream.on('error',()=>socket.destroy());socket.on('error',()=>upstream.destroy());socket.on('close',()=>upstream.destroy());
  });proxy.on('connection',socket=>{transports.add(socket);socket.on('close',()=>transports.delete(socket));});proxy.listen(0,'127.0.0.1');await once(proxy,'listening');
  if(!process.env.CHROME)throw Error();
  browser=await chromium.launch({executablePath:process.env.CHROME,args:['--no-sandbox',`--ignore-certificate-errors-spki-list=${certificatePin}`],proxy:{server:`http://127.0.0.1:${proxy.address().port}`}});
  const context=await browser.newContext({viewport:{width:466,height:678}});
  page=await context.newPage();page.setDefaultTimeout(0);page.setDefaultNavigationTimeout(0);
  page.on('pageerror',()=>console.error('Native client script boundary failed'));
  await context.exposeBinding('__nativeProgress',async({frame},state)=>{
    if(frame!==page.mainFrame())throw Error();
    await writeFile(resolvePath(evidence,'main-visible-state.json'),JSON.stringify(state,null,2)+'\n');
  });
  await context.addInitScript({content:`document.addEventListener('DOMContentLoaded',()=>{
    let previous='';
    const report=()=>{
      const visible=selector=>{const node=document.querySelector(selector);return !!node&&node.getBoundingClientRect().height>0;};
      const state={connected:visible('[data-mobile-test="connection-banner-dot-connected"]'),connecting:visible('[data-mobile-test="connection-banner-spinner"]'),
        bootstrapping:visible('[data-mobile-test="connection-banner-bootstrapping"]'),repair:visible('[data-mobile-test="connection-banner-repair"]'),
        update:visible('[data-mobile-test="connection-banner-update-required"]'),shell_error:visible('[data-mobile-test="mobile-shell-error-banner"]'),
        agents:visible('[data-mobile-test="agents-view"]'),sessions:visible('[data-mobile-test="sessions-view"]'),foreground:document.visibilityState==='visible'};
      const next=JSON.stringify(state);if(next!==previous){previous=next;void window.__nativeProgress(state);}
    };
    new MutationObserver(report).observe(document.body,{subtree:true,childList:true,attributes:true});document.addEventListener('visibilitychange',report);report();
  });`});
  page.on('requestfailed',request=>console.error(`Native request failed; type=${request.resourceType()}`));
  const boundary=async phase=>{
    stage=phase;console.error(`NATIVE_BOUNDARY ${phase}`);
    await writeFile(resolvePath(evidence,'current-boundary.json'),JSON.stringify({stage})+'\n');
  };
  const renderedDesktopQr=async svg=>{
    const desktop=await browser.newPage({viewport:{width:1200,height:900}});
    desktop.setDefaultTimeout(0);desktop.setDefaultNavigationTimeout(0);
    try{
      await desktop.setContent('<html data-theme="dark"><body><div class="settings-overlay"><div class="settings-root"><div class="settings-layout"><div class="settings-content"><div class="settings-mobile-pairing-active"><div class="settings-mobile-pairing-qr"></div></div></div></div></div></div></body></html>');
      await desktop.addStyleTag({content:desktopStyles});
      const qr=desktop.locator('.settings-mobile-pairing-qr');
      await qr.evaluate((node,svg)=>{node.innerHTML=svg;},svg);
      await desktop.evaluate(()=>Promise.all(document.getAnimations().map(animation=>animation.finished)));
      const box=await qr.boundingBox();
      if(!box||box.width<=0||box.height<=0)throw Error('Desktop pairing QR is not rendered');
      const layout=await qr.evaluate(node=>{
        const svg=node.querySelector('svg').getBoundingClientRect(),box=node.getBoundingClientRect();
        let visible={left:Math.max(0,box.left),top:Math.max(0,box.top),right:Math.min(innerWidth,box.right),bottom:Math.min(innerHeight,box.bottom)};
        for(let parent=node.parentElement;parent;parent=parent.parentElement){
          const style=getComputedStyle(parent),rect=parent.getBoundingClientRect();
          if(['hidden','auto','scroll','clip'].includes(style.overflowX)){visible.left=Math.max(visible.left,rect.left);visible.right=Math.min(visible.right,rect.right);}
          if(['hidden','auto','scroll','clip'].includes(style.overflowY)){visible.top=Math.max(visible.top,rect.top);visible.bottom=Math.min(visible.bottom,rect.bottom);}
        }
        return {svg_width:svg.width,svg_height:svg.height,svg_x:svg.left-box.left,svg_y:svg.top-box.top,
          fully_visible:visible.left<=box.left&&visible.top<=box.top&&visible.right>=box.right&&visible.bottom>=box.bottom,
          opacity:getComputedStyle(node.closest('.settings-overlay')).opacity};
      });
      const pixels=await qr.screenshot();
      return {png:pixels.toString('base64'),width:box.width,height:box.height,...layout};
    }finally{await desktop.close();}
  };
  const scanRenderedDesktopQr=async (target,host)=>{
    const rendered=await renderedDesktopQr(host.qr_svg);
    const receipt=resolvePath(evidence,`desktop-qr-${host.phase||'direct'}-decode-boundary.json`);
    let completeVideo;
    const videoFrame=new Promise(resolve=>{completeVideo=resolve;});
    const videoObservation={events:[]};let videoWrites=Promise.resolve();
    await target.exposeBinding('__thinQrVideoDiagnostic',async ({frame},snapshot)=>{
      if(frame!==target.mainFrame())throw Error('Untrusted QR diagnostic frame');
      const {pixels,...state}=snapshot;
      videoObservation.events.push(state);
      if(pixels)videoObservation.decoder_pipeline=diagnoseQrPipeline(pixels,host.qr_uri);
      const json=JSON.stringify(videoObservation,null,2)+'\n';
      videoWrites=videoWrites.then(()=>writeFile(resolvePath(evidence,`desktop-qr-${host.phase||'direct'}-video-boundary.json`),json));
      await videoWrites;
      if(pixels||state.scanner_failure)completeVideo(state);
    });
    let decoded;
    try{
    decoded=await target.evaluate(async ({png,uri,svg,layout})=>{
      const observation=window.__thinQrDecodeObservation={stage:'import'};
      const {decodeQrImageData,isPairingScanValue}=await import('/tyde/loader.js');
      observation.stage='image';
      const image=new Image();image.src='data:image/png;base64,'+png;await image.decode();
      const canvas=document.createElement('canvas');canvas.width=image.naturalWidth;canvas.height=image.naturalHeight;
      const context=canvas.getContext('2d');context.drawImage(image,0,0);
      const frame=context.getImageData(0,0,canvas.width,canvas.height);
      let minimum=255,maximum=0;
      for(let i=0;i<frame.data.length;i+=4){minimum=Math.min(minimum,frame.data[i]);maximum=Math.max(maximum,frame.data[i]);}
      Object.assign(observation,{stage:'decode',width:canvas.width,height:canvas.height,minimum,maximum});
      const value=decodeQrImageData(frame);
      const exact=value===uri,recognized=isPairingScanValue(value);
      Object.assign(observation,{decoded:typeof value==='string',exact,recognized});
      const vector=new DOMParser().parseFromString(svg,'image/svg+xml').documentElement;
      const dimension=vector.viewBox.baseVal.width;
      const dark=new Set([...vector.querySelectorAll('rect[fill="#000000"]')].map(node=>node.getAttribute('x')+':'+node.getAttribute('y')));
      let darkMismatch=0,lightMismatch=0;
      for(let y=0;y<dimension;y++)for(let x=0;x<dimension;x++){
        const px=Math.floor(layout.svg_x+(x+.5)*layout.svg_width/dimension);
        const py=Math.floor(layout.svg_y+(y+.5)*layout.svg_height/dimension);
        const expected=dark.has(x+':'+y),actual=frame.data[(py*frame.width+px)*4]<128;
        if(expected!==actual){if(expected)darkMismatch++;else lightMismatch++;}
      }
      observation.modules={dimension,compared:dimension*dimension,dark_mismatch:darkMismatch,light_mismatch:lightMismatch};
      window.__thinQrDebugFrame={data:Array.from(frame.data),width:frame.width,height:frame.height,dimension,...layout,
        bits:Array.from({length:dimension*dimension},(_,i)=>dark.has(i%dimension+':'+Math.floor(i/dimension))?1:0)};
      const diagnose=(label,source,sx,sy,sw,sh,width,height,vector=false)=>{
        const sample=document.createElement('canvas');sample.width=width;sample.height=height;
        const pixels=sample.getContext('2d');
        if(vector)pixels.drawImage(source,0,0,width,height);else pixels.drawImage(source,sx,sy,sw,sh,0,0,width,height);
        const text=decodeQrImageData(pixels.getImageData(0,0,width,height));
        return {path:label,width,height,decoded:typeof text==='string',exact:text===uri,recognized:isPairingScanValue(text)};
      };
      // These comparisons diagnose the same private offer; none substitutes for
      // the original screenshot's acceptance or its production Scan input.
      const comparisons=[diagnose('screenshot_svg_crop',image,layout.svg_x,layout.svg_y,layout.svg_width,layout.svg_height,layout.svg_width,layout.svg_height)];
      const vectorUrl=URL.createObjectURL(new Blob([svg],{type:'image/svg+xml'}));
      try{
        const vectorImage=new Image();vectorImage.src=vectorUrl;await vectorImage.decode();
        for(const size of [layout.svg_width,600])comparisons.push(diagnose('same_offer_svg',vectorImage,0,0,vectorImage.naturalWidth,vectorImage.naturalHeight,size,size,true));
      }finally{URL.revokeObjectURL(vectorUrl);}
      observation.comparisons=comparisons;
      if(!exact||!recognized)throw Error('Rendered desktop QR did not pass the actual native loader recognizer');
      // Actual media frames exercise the scanner consumer, not camera permission/capture.
      const stream=canvas.captureStream(0),request=navigator.mediaDevices.getUserMedia;
      const track=stream.getVideoTracks()[0];
      if(typeof track.requestFrame!=='function')throw Error('Explicit canvas video frame capture is unavailable');
      const decoder=window.jsQR,template=window.__thinQrDebugFrame,video=document.getElementById('scan-video');
      const report=(event,extra={})=>window.__thinQrVideoDiagnostic({event,ready_state:video.readyState,paused:video.paused,
        width:video.videoWidth,height:video.videoHeight,live_tracks:stream.getTracks().filter(track=>track.readyState==='live').length,...extra});
      const listeners=['play','loadedmetadata','loadeddata','playing','error'].map(event=>{
        const listener=()=>{
          // The detached canvas must paint after capture starts; otherwise the
          // stream stays live with zero dimensions and play never settles.
          if(event==='play'){context.drawImage(image,0,0);track.requestFrame();}
          void report(event);
        };video.addEventListener(event,listener);return [event,listener];
      });
      const status=new MutationObserver(()=>{if(document.getElementById('scan-status').getAttribute('role')==='alert')void report('scanner-error',{scanner_failure:true});});
      status.observe(document.getElementById('scan-status'),{childList:true,attributes:true});
      navigator.mediaDevices.getUserMedia=async constraints=>{await report('camera-request',{environment_requested:constraints?.video?.facingMode==='environment'});return stream;};
      let observed=false;
      window.jsQR=(data,width,height,options)=>{
        const result=decoder(data,width,height,options);
        if(!observed){observed=true;void report('decoder-frame',{decoded:typeof result?.data==='string',exact:result?.data===uri,
          recognized:isPairingScanValue(result?.data),pixels:{...template,data:Array.from(data),width,height}});}
        return result;
      };
      window.__thinQrSource={canvas,stream,restore(){navigator.mediaDevices.getUserMedia=request;window.jsQR=decoder;
        status.disconnect();for(const [event,listener]of listeners)video.removeEventListener(event,listener);}};
      return {exact,recognized};
    },{png:rendered.png,uri:host.qr_uri,svg:host.qr_svg,layout:{svg_x:rendered.svg_x,svg_y:rendered.svg_y,svg_width:rendered.svg_width,svg_height:rendered.svg_height}});
    }finally{
      const observation=await target.evaluate(()=>window.__thinQrDecodeObservation);
      const frame=await target.evaluate(()=>window.__thinQrDebugFrame);
      if(frame)observation.decoder_pipeline=diagnoseQrPipeline(frame,host.qr_uri);
      await target.evaluate(()=>{delete window.__thinQrDebugFrame;});
      await writeFile(receipt,JSON.stringify({rendered_width:rendered.width,rendered_height:rendered.height,
        svg_width:rendered.svg_width,svg_height:rendered.svg_height,svg_x:rendered.svg_x,svg_y:rendered.svg_y,fully_visible:rendered.fully_visible,opacity:rendered.opacity,...observation},null,2)+'\n');
    }
    await target.locator('#scan-button').click();
    const video=await videoFrame;
    if(video.scanner_failure)throw Error('The actual scanner surfaced a camera or decoder failure');
    if(!video.exact||!video.recognized)throw Error('The actual scanner did not decode its supplied QR video frame');
    return {desktop_producer:true,desktop_css_dom_pixels:true,rendered_width:rendered.width,rendered_height:rendered.height,
      real_server_offer:true,decoded_exact:decoded.exact,recognizer_accepted:decoded.recognized,
      limit:'Production desktop QR renderer and CSS in an isolated real DOM fixture, not the mounted Settings component; rendered screenshot pixels feed a canvas stream and actual native-loader continuation, not a physical camera or UIKit.'};
  };
  const finishQrCapture=async target=>{
    const stopped=await target.evaluate(()=>{
      const source=window.__thinQrSource;
      const stopped=source.stream.getTracks().every(track=>track.readyState==='ended');
      source.restore();delete window.__thinQrSource;return stopped;
    });
    if(!stopped)throw Error('The native loader did not stop its consumed QR stream');
  };
  const requireVisible=async selector=>Promise.race([
    page.locator(selector).waitFor({state:'visible'}),
    page.locator('#view-error').waitFor({state:'visible'}).then(()=>{throw Error('Explicit bootstrap failure');}),
  ]);
  await context.exposeBinding('__nativePlatform',async({frame},endpoint,encoded)=>{
    if(frame!==page.mainFrame())throw Error('Untrusted native frame');
    const body=JSON.parse(encoded);
    if(endpoint==='begin_bootstrap'){documents++;scope=null;for(const request of pendingHttp.values())request.destroy();pendingHttp.clear();return null;}
    requireRequest(endpoint,body);
    if(endpoint==='presentation'){
      presentation=body;
      await frame.evaluate(value=>window.__thinPresentation=value,body);
      await writeFile(resolvePath(evidence,'main-presentation-state.json'),JSON.stringify({workspace:body.workspace,tab:body.tab,route:body.route,modal:body.modal},null,2)+'\n');
      return null;
    }
    if(endpoint==='secret_scope'){
      if(scope&&body.keys.some(key=>!scope.has(key)))throw Error('Secret scope cannot expand');scope=new Set(body.keys);return null;
    }
    if(endpoint==='secret_read'){if(scope&&!scope.has(body.key))throw Error('Secret scope violation');return secrets.get(body.key)??null;}
    if(endpoint==='secret_write'){if(scope&&!scope.has(body.key)&&secrets.has(body.key))throw Error('Secret replacement denied');scope?.add(body.key);secrets.set(body.key,body.value);return null;}
    if(endpoint==='secret_delete'){if(scope&&!scope.has(body.key))throw Error('Secret scope violation');secrets.delete(body.key);return null;}
    if(endpoint==='http_cancel'){pendingHttp.get(body.id)?.destroy();return null;}
    if(endpoint!=='http')throw Error('Unsupported native platform operation');
    if(closing)throw Error('Owned native document closed');
    const url=new URL(body.url),address=targets.get(url.hostname);
    if(!address||url.protocol!=='https:')throw Error('Unexpected native HTTP destination');
    if(url.pathname==='/tyde/pair')pairRequests++;
    const discovery=url.hostname==='native-host.test'&&url.pathname==='/tyde/bootstrap';
    if(discovery){mainDiscovery.requests++;if(Object.keys(body.headers).some(name=>name.toLowerCase()==='authorization'))mainDiscovery.device_requests++;if(networkDrops>0)rediscovered?.();}
    return new Promise((resolve,reject)=>{
      const request=httpRequest(`http://${address}${url.pathname}`,{method:body.method.toUpperCase(),headers:body.headers},response=>{
        let data='';response.on('data',part=>data+=part);response.on('end',async()=>{
          pendingHttp.delete(body.id);
          if(discovery){mainDiscovery.responses++;mainDiscovery.last_status=response.statusCode;
            await writeFile(resolvePath(evidence,'main-discovery.json'),JSON.stringify(mainDiscovery,null,2)+'\n');}

          if(url.hostname==='native-host.test'&&url.pathname==='/tyde/bootstrap'&&response.statusCode>=500){offlineCount++;offlineObserved?.();}
          resolve({status:response.statusCode,body:data});
        });
      });requests.add(request);request.on('close',()=>requests.delete(request));pendingHttp.set(body.id,request);request.on('error',()=>{pendingHttp.delete(body.id);if(url.hostname==='native-host.test'&&url.pathname==='/tyde/bootstrap'){offlineCount++;offlineObserved?.();}reject(Error('Native HTTP failed'));});request.end(body.body);
    });
  });
  await context.addInitScript({content:`if(location.origin==='https://tycode.dev'){
    const monotonicNow=performance.now.bind(performance);let clockOffset=0;
    Object.defineProperty(performance,'now',{value:()=>monotonicNow()+clockOffset});
    window.__thinAdvanceClock=delta=>clockOffset+=delta;
    const ready=window.__nativePlatform('begin_bootstrap','{}');
    window.webkit={messageHandlers:Object.fromEntries(${JSON.stringify(Object.keys(contract.requests))}.map(endpoint=>[endpoint,{postMessage:body=>ready.then(()=>window.__nativePlatform(endpoint,body))}]))};
    const tydeNativeBridgeVersion=${metadata.bridge_version},tydeNativeBootstrapVersion=${metadata.bootstrap_version},tydeNativeShellRelease=${JSON.stringify(metadata.release)};
    const tydeNativeSecretScopeEndpoint='secret_scope',tydeNativeHttpEndpoint='http',tydeNativeHttpCancelEndpoint='http_cancel';
    ${bridgeScript}
  }`});
  const intent=detail=>{if(!accepts(detail,contract.intents))throw Error('The installed native shell cannot emit this intent');return page.evaluate(detail=>window.dispatchEvent(new CustomEvent('tyde-native-intent',{detail})),detail);};
  await page.exposeBinding('__thinPairingSettled',(_,outcome)=>{
    if(pairingOutcome){pairingOutcome(outcome);pairingOutcome=null;}
  });
  await page.exposeBinding('__thinPairingCancellationObserved',()=>{
    if(pairingCancellationObserved){pairingCancellationObserved();pairingCancellationObserved=null;}
  });
  const identity=()=>page.evaluate(()=>new Promise((resolve,reject)=>{
    const open=indexedDB.open('tyde-mobile',1);open.onerror=()=>reject(Error());open.onsuccess=()=>{
      const db=open.result,tx=db.transaction('paired_hosts'),get=tx.objectStore('paired_hosts').get('all');
      get.onsuccess=()=>resolve(JSON.parse(get.result).map(x=>[x.localHostId,x.direct?.deviceId,x.direct?.deviceTokenKeyId,x.credentialFingerprint,x.direct?.nativeOrigin]));tx.oncomplete=()=>db.close();
    };
  }));
  const loaded=async release=>{
    await Promise.race([(async()=>{
      await page.waitForFunction(release=>window.__tydeLoader?.bootVersion()===release,release);
      await page.locator('[data-mobile-test="connection-banner-dot-connected"]').waitFor({state:'visible'});
    })(),page.locator('[data-mobile-test="connection-banner-update-required"], [data-mobile-test="mobile-shell-error-banner"], [data-mobile-test="connection-banner-repair"]').first().waitFor({state:'visible'}).then(()=>{throw Error('Client refused the current authenticated target');})]);
  };
  const chat=async(title,prompt,reply,phase)=>{
    await boundary(`${phase} conversation navigation`);
    await intent('agents');
    await page.waitForFunction(()=>window.__thinPresentation?.tab==='agents');
    // Selecting Agents restores its retained conversation, so its real Back
    // action is needed after selecting the tab, not while Home is visible.
    if(await page.evaluate(()=>window.__thinPresentation?.route==='chat'))await intent('back');
    await page.locator('[data-mobile-test="agents-view"]').getByRole('button',{name:'Open chat with '+title,exact:true}).click();
    await boundary(`${phase} conversation submission`);
    await page.locator('[data-mobile-test="chat-input"]').fill(prompt);await page.locator('[data-mobile-test="chat-send"]').click();
    await boundary(`${phase} authoritative reply and idle`);
    await page.waitForFunction(reply=>document.querySelector('[data-mobile-test="chat-messages"]')?.textContent.includes(reply),reply);
    await page.waitForFunction(()=>document.querySelector('[data-mobile-test="chat-input"]')?.value===''&&document.querySelector('[data-mobile-test="chat-send"]')?.getAttribute('aria-label')==='Send message');
  };
  const managedContexts=[];
  const managedProbe=async host=>{
    await boundary(`real Service ${host.phase} redemption`);
    const probeContext=await browser.newContext({viewport:{width:466,height:678}});
    const probe=await probeContext.newPage();probe.setDefaultTimeout(0);probe.setDefaultNavigationTimeout(0);
    const keys=new Map();let secretScope=null, attempts=0, responses=0, publicRequest=null, includeSession=true;
    const resolutions=[],httpReceipts=[];let nextDiscovery=null,discoveryStarted=0;
    await probeContext.exposeBinding('__nativePlatform',async({frame},endpoint,encoded)=>{
      if(frame!==probe.mainFrame())throw Error();const body=JSON.parse(encoded);
      if(endpoint==='begin_bootstrap'){secretScope=null;return null;}
      requireRequest(endpoint,body);
      if(endpoint==='presentation'){await frame.evaluate(value=>window.__thinPresentation=value,body);return null;}
      if(endpoint==='secret_scope'){if(secretScope&&body.keys.some(key=>!secretScope.has(key)))throw Error();secretScope=new Set(body.keys);return null;}
      if(endpoint==='secret_read'){if(secretScope&&!secretScope.has(body.key))throw Error();return keys.get(body.key)??null;}
      if(endpoint==='secret_write'){if(secretScope&&!secretScope.has(body.key)&&keys.has(body.key))throw Error();secretScope?.add(body.key);keys.set(body.key,body.value);return null;}
      if(endpoint==='secret_delete'){if(secretScope&&!secretScope.has(body.key))throw Error('Secret scope violation');keys.delete(body.key);return null;}
      if(endpoint==='http_cancel')return null;
      if(endpoint!=='http')throw Error();
      if(closing)throw Error('Owned native document closed');
      const url=new URL(body.url);if(url.origin!=='https://tycode.dev'||!url.pathname.startsWith('/api/tyde/mobile/v1/'))throw Error();
      const redeem=url.pathname.endsWith('/pairings/redeem');if(redeem)attempts++;
      const discoveryOrdinal=url.pathname.endsWith('/bootstrap/resolve')?++discoveryStarted:0;
      const target=new URL(initial.service.endpoint).origin+url.pathname;
      return new Promise((resolve,reject)=>{
        const request=httpRequest(target,{method:body.method.toUpperCase(),headers:{...body.headers,...(includeSession?{cookie:initial.service.cookie}:{})}},response=>{
          let data='';response.on('data',part=>data+=part);response.on('end',async()=>{
            const kind=redeem?'redeem':url.pathname.endsWith('/bootstrap/resolve')?'bootstrap_resolve':url.pathname.endsWith('/auth/session')?'session':'other_managed_http';
            let responseBody=null;try{responseBody=JSON.parse(data);}catch{}
            const code=responseBody?.error?.code;
            httpReceipts.push({kind,http_status:response.statusCode,...(typeof code==='string'&&/^[a-z_]{1,64}$/.test(code)?{error_code:code}:{})});
            if(kind==='bootstrap_resolve'&&nextDiscovery&&discoveryOrdinal>nextDiscovery.after){const completed=nextDiscovery.complete;nextDiscovery=null;completed(httpReceipts.at(-1));}
            if(redeem){
              responses++;const request=JSON.parse(body.body);
              publicRequest={release:request.release_version,protocol:request.protocol_version,transport:request.transport_protocol_version,http_status:response.statusCode};
              if(host.phase==='failed_registration')await frame.evaluate(()=>window.__thinAdvanceClock(121000));
            }
            await writeFile(resolvePath(evidence,`managed-${host.phase}-http-receipt.json`),JSON.stringify({shell_archive_sha256:custody.archive_sha256,service_commit:initial.service.service_commit,
              actual_native_request:publicRequest,request_body_unchanged:true,redeem_starts:attempts,redeem_responses:responses,http: httpReceipts,canonical_completion:false},null,2)+'\n');
            if(url.pathname.endsWith('/bootstrap/resolve')&&response.statusCode===200){
              const resolved=JSON.parse(data);resolutions.push({kind:resolved.binding.kind,descriptor:resolved.descriptor});
            }
            resolve({status:response.statusCode,body:data});
          });
        });requests.add(request);request.on('close',()=>requests.delete(request));request.on('error',()=>reject(Error('Local Service HTTP failed')));request.end(body.body);
      });
    });
    await probeContext.addInitScript({content:`if(location.origin==='https://tycode.dev'){
      const monotonicNow=performance.now.bind(performance);let clockOffset=0;
      Object.defineProperty(performance,'now',{value:()=>monotonicNow()+clockOffset});
      window.__thinAdvanceClock=delta=>clockOffset+=delta;
      const ready=window.__nativePlatform('begin_bootstrap','{}');
      window.webkit={messageHandlers:Object.fromEntries(${JSON.stringify(Object.keys(contract.requests))}.map(endpoint=>[endpoint,{postMessage:body=>ready.then(()=>window.__nativePlatform(endpoint,body))}]))};
      const tydeNativeBridgeVersion=${metadata.bridge_version},tydeNativeBootstrapVersion=${metadata.bootstrap_version},tydeNativeShellRelease=${JSON.stringify(metadata.release)};
      const tydeNativeSecretScopeEndpoint='secret_scope',tydeNativeHttpEndpoint='http',tydeNativeHttpCancelEndpoint='http_cancel';
      ${bridgeScript}
    }`});
    const rows=()=>probe.evaluate(()=>new Promise((resolve,reject)=>{
      const open=indexedDB.open('tyde-mobile',1);open.onerror=reject;open.onsuccess=()=>{
        const db=open.result,tx=db.transaction('paired_hosts'),get=tx.objectStore('paired_hosts').get('all');
        get.onsuccess=()=>resolve(JSON.parse(get.result));tx.oncomplete=()=>db.close();
      };
    }));
    try {
    await probe.goto('https://tycode.dev/tyde/?native=1',{waitUntil:'domcontentloaded'});
    let qrProof;
    if(host.phase==='before')qrProof=await scanRenderedDesktopQr(probe,host);
    else{await probe.locator('#paste-input').fill(host.qr_uri);await probe.locator('#connect-button').click();}
    // Managed ServiceAuthScreen performs its existing authenticated redemption;
    // only direct pairing has a separate Pair confirmation button.
    await Promise.race([probe.waitForFunction(()=>window.__thinPresentation?.workspace===true),probe.locator('[data-mobile-test="pairing-failed-message"]').waitFor({state:'visible'}).then(()=>{throw Error();})]);
    const saved=await rows();if(saved.length!==1||!saved[0].managed||keys.size!==2||attempts!==1||responses!==1||publicRequest.http_status!==200||publicRequest.release!==host.release||publicRequest.protocol!==published.versions[host.release].protocolVersion||publicRequest.transport!==published.versions[host.release].transportProtocolVersion)throw Error();
    if(qrProof){
      await finishQrCapture(probe);
      await writeFile(resolvePath(evidence,'managed-desktop-qr-native-continuation.json'),JSON.stringify({
        ...qrProof,canonical_managed_redemption:true,stream_stopped:true,redeem_starts:attempts,redeem_responses:responses,
      },null,2)+'\n');
    }
    emit({event:'managed-saved',pairing_id:saved[0].managed.pairingId,device_id:saved[0].managed.deviceId});
    const registration=await receive();
    if(host.phase==='failed_registration'){
      if(registration.event!=='managed-registration-failed')throw Error();
      // VersionMismatch is a terminal Error, not auth/repair NeedsAction.
      // Require its real alert and Reconnect; never change product status for this harness.
      await probe.locator('[data-mobile-test="connection-banner"] [role="alert"]').filter({hasText:'update Tyde host'}).waitFor({state:'visible'});
      if(!await probe.locator('[data-mobile-test="connection-banner-reconnect"]').isVisible()
          ||await probe.locator('[data-mobile-test="connection-banner-repair"]').isVisible()
          ||await probe.locator('[data-mobile-test="connection-banner-spinner"]').isVisible())throw Error('Registration failure lost terminal reconnect recovery');
      if(attempts!==1||responses!==1||!httpReceipts.some(r=>r.kind==='bootstrap_resolve'&&r.error_code==='bootstrap_unknown'))throw Error('Missing registration lost its explicit terminal result');
      await writeFile(resolvePath(evidence,'failed-paired-registration-proof.json'),JSON.stringify({
        actual_host_registration_http_status:503,resource_scoped_by_actual_offer_poll_binding:true,
        actual_service_bootstrap_unknown:true,terminal_host_registration_error:true,offers_reconnect_not_repair:true,network_outage_inferred:false,
        injected_monotonic_advance_ms:121000,redeem_starts:attempts,redeem_responses:responses,canonical_completion:true,
        record_and_secrets_retained:true,shell_archive_sha256:custody.archive_sha256,
        limit:'Real local Service, actual native client/HTTP and controlled producer transport failure; no RTC or Swift runtime claim.'
      },null,2)+'\n');
      await probe.screenshot({path:resolvePath(evidence,'failed-paired-registration.png')});
      emit({event:'managed-verified'});managedContexts.push({context:probeContext});return;
    }
    if(registration.event!=='managed-registered')throw Error();
    const reload=async(release,phase)=>{
      await boundary(`real Service retained ${phase} resolution`);await probe.reload({waitUntil:'domcontentloaded'});
      await Promise.race([probe.waitForFunction(release=>window.__tydeLoader?.bootVersion()===release,release),probe.locator('#view-error').waitFor({state:'visible'}).then(()=>{throw Error();})]);
      await probe.waitForFunction(()=>window.__thinPresentation?.workspace===true);
      if(JSON.stringify(await rows())!==JSON.stringify(saved)||keys.size!==2||attempts!==1||responses!==1||!resolutions.some(value=>value.kind==='pairing'&&value.descriptor.client_release_version===release))throw Error();
      await writeFile(resolvePath(evidence,`managed-${host.phase}-${phase}-proof.json`),JSON.stringify({shell_archive_sha256:custody.archive_sha256,service_commit:initial.service.service_commit,
        actual_native_request:publicRequest,request_body_unchanged:true,redeem_starts:attempts,redeem_responses:responses,canonical_completion:true,record_and_secrets_retained:true,
        resolutions,limit:'Real local Service/PostgreSQL with fixture session; actual host assertions and source-built client assets. No RTC/TURN, OAuth provider, Swift/Keychain or production-auth proof.'},null,2)+'\n');
      await probe.screenshot({path:resolvePath(evidence,`managed-${host.phase}-${phase}.png`)});
    };
    await reload(host.release,'registered');
    if(host.phase==='after'){
      await boundary('unauthenticated in-app pairing bootstrap handoff');
      await probe.evaluate(()=>window.dispatchEvent(new CustomEvent('tyde-native-intent',{detail:'hosts'})));
      await probe.getByRole('button',{name:'Pair another host',exact:true}).click();
      await probe.getByRole('button',{name:'Paste link',exact:true}).click();
      await probe.getByRole('textbox',{name:'Pairing link',exact:true}).fill(host.qr_uri);
      const unauthorizedBefore=httpReceipts.filter(value=>value.kind==='bootstrap_resolve'&&value.http_status===401).length;
      includeSession=false;
      await probe.getByRole('button',{name:'Continue',exact:true}).click();
      await probe.locator('#view-error').waitFor({state:'visible'});
      await probe.getByRole('button',{name:'Continue with Apple',exact:true}).waitFor({state:'visible'});
      await probe.getByRole('button',{name:'Continue with Google',exact:true}).waitFor({state:'visible'});
      if(attempts!==1||responses!==1||JSON.stringify(await rows())!==JSON.stringify(saved)||keys.size!==2||
          httpReceipts.filter(value=>value.kind==='bootstrap_resolve'&&value.http_status===401).length<=unauthorizedBefore||
          await probe.evaluate(()=>window.__tydeLoader?.bootVersion())!==null)throw Error('In-app sign-in bypassed bootstrap or consumed another offer');
      await probe.screenshot({path:resolvePath(evidence,'managed-in-app-pairing-auth-required.png')});
      includeSession=true;
      await probe.getByRole('button',{name:'Hosts',exact:true}).click();
      await probe.getByRole('button').filter({hasText:saved[0].hostLabel}).click();
      await probe.waitForFunction(release=>window.__tydeLoader?.bootVersion()===release,host.release);
      await probe.waitForFunction(()=>window.__thinPresentation?.workspace===true);
      if(attempts!==1||responses!==1||JSON.stringify(await rows())!==JSON.stringify(saved)||keys.size!==2)throw Error('Cancelled sign-in lost the existing pairing');
      await writeFile(resolvePath(evidence,'managed-in-app-pairing-auth-proof.json'),JSON.stringify({actual_service_401:true,explicit_safe_pairing_handoff:true,
        canonical_bootstrap_sign_in_actions:true,no_full_client_before_authenticated_resolution:true,retained_identity:true,redeem_starts:attempts,redeem_responses:responses,
        limit:'Real Service401 and full DOM flow; existing fixture session restored to cancel and reopen retained host. No OAuth provider or Swift authentication-session execution.'},null,2)+'\n');
    }
    if(host.phase==='before'){
      includeSession=false;
      await probe.locator('[data-mobile-test="connection-banner-sign-in"]').waitFor({state:'visible'});
      if(await probe.locator('[data-mobile-test="connection-banner-repair"]').isVisible()||attempts!==1)throw Error('Session expiry became re-pair');
      await probe.screenshot({path:resolvePath(evidence,'managed-session-auth-required.png')});
      if(!httpReceipts.some(receipt=>receipt.kind==='bootstrap_resolve'&&receipt.http_status===401))throw Error();
      includeSession=true;await reload(host.release,'authenticated-recovery');
      await writeFile(resolvePath(evidence,'managed-auth-recovery-proof.json'),JSON.stringify({actual_service_unauthenticated_401:true,full_client_sign_in_action:true,no_repair_action:true,retained_identity:true,redeem_starts:attempts,limit:'Fixture session restored for explicit reload; no OAuth provider or RTC claim.'},null,2)+'\n');
    }
    emit({event:'managed-verified'});
    const verifyRevoked=async()=>{
      await boundary('managed owner revocation on actual reconnect');
      const discovery=new Promise(resolve=>nextDiscovery={after:discoveryStarted,complete:resolve});
      const repair=probe.locator('[data-mobile-test="connection-banner-repair"]');
      await Promise.race([
        repair.waitFor({state:'visible'}),
        (async()=>{
          await probe.locator('[data-mobile-test="connection-banner-reconnect"]').click();
          const receipt=await discovery;
          if(receipt.error_code!=='pairing_revoked')throw Error('Owner removal left managed authority active');
          await repair.waitFor({state:'visible'});
        })()
      ]);
      if(!httpReceipts.some(r=>r.kind==='bootstrap_resolve'&&r.error_code==='pairing_revoked')||
          await probe.locator('[data-mobile-test="connection-banner-spinner"]').isVisible()||
          await probe.locator('[data-mobile-test="connection-banner-sign-in"]').isVisible()||
          JSON.stringify(await rows())!==JSON.stringify(saved)||keys.size!==2||attempts!==1)throw Error('Managed revocation did not preserve identity and render authoritative repair');
      await probe.screenshot({path:resolvePath(evidence,'managed-revoked-access.png')});
      await writeFile(resolvePath(evidence,'managed-revoked-access-proof.json'),JSON.stringify({
        real_owner_revocation:true,actual_service_pairing_revoked:true,existing_repair_action:true,
        spinner_absent:true,sign_in_absent:true,record_and_secrets_retained:true,extra_pairing:false,
        synchronization:'actual HTTP rejection and rendered repair action; no timeout threshold',
        limit:'Real local Service, host protocol and source-built Wasm with frozen native ABI. Not Swift, real OAuth or physical phone proof.'
      },null,2)+'\n');
    };
    const result={reload,verifyRevoked,context:probeContext};managedContexts.push(result);return result;
    } catch {
      const state=await probe.evaluate(()=>({workspace:window.__thinPresentation?.workspace===true,client:window.__tydeLoader?.bootVersion(),
        pairingFailed:!!document.querySelector('[data-mobile-test="pairing-failed-message"]'),shellError:!!document.querySelector('[data-mobile-test="mobile-shell-error-banner"]'),
        loaderFailed:!document.getElementById('view-error')?.hidden}));
      await writeFile(resolvePath(evidence,`managed-${host.phase}-failure.json`),JSON.stringify({stage,...state,redeem_starts:attempts,redeem_responses:responses,secrets:keys.size,record_count:(await rows()).length},null,2)+'\n');
      await probe.evaluate(()=>{document.querySelectorAll('textarea,input').forEach(node=>node.value='');const video=document.querySelector('#scan-video');if(video)video.hidden=true;});
      await probe.screenshot({path:resolvePath(evidence,`managed-${host.phase}-failure-redacted.png`)});throw Error();
    }
  };
  stage='first actual client';console.error('NATIVE_BOUNDARY first client ready');emit({event:'ready',publisher:{proxy:`http://127.0.0.1:${proxy.address().port}`,certificate:await readFile(resolvePath(temporary,'cert.pem'),'utf8')}});
  if((await receive()).event!=='release-publisher'||!releasePublisher)throw Error('Missing publication boundary');
  releasePublisher();emit({event:'publisher-released'});
  const first=await receive();targets.set('native-host.test',first.address);
  await boundary('trusted publisher navigation');
  await page.goto('https://tycode.dev/tyde/?native=1',{waitUntil:'domcontentloaded'});
  await boundary('bootstrap pairing chooser');
  await requireVisible('#paste-input');
  await boundary('bootstrap chooser screenshot');
  await page.screenshot({path:resolvePath(evidence,'bootstrap-chooser.png')});
  await boundary('desktop QR producer and real decoder to native scanner continuation');
  const qrProof=await scanRenderedDesktopQr(page,first);
  console.error('NATIVE_BOUNDARY awaiting first Pair');
  await Promise.race([page.getByRole('button',{name:'Pair',exact:true}).waitFor({state:'visible'}),page.locator('#view-error').waitFor({state:'visible'}).then(()=>{throw Error();})]);
  await finishQrCapture(page);
  await writeFile(resolvePath(evidence,'desktop-qr-native-continuation.json'),JSON.stringify({
    ...qrProof,canonical_pair_confirmation:true,stream_stopped:true,
  },null,2)+'\n');
  const discoveryBeforeConfirmation=mainDiscovery.device_requests;
  await page.evaluate(()=>window.__thinAdvanceClock(121000));
  await page.getByRole('button',{name:'Pair',exact:true}).click();
  console.error('NATIVE_BOUNDARY awaiting first bootstrap');
  await loaded(first.release);
  if(mainDiscovery.device_requests!==discoveryBeforeConfirmation+1||pairRequests!==1)throw Error('Expired confirmation did not renew paired authority exactly once');
  await writeFile(resolvePath(evidence,'expired-confirmation-proof.json'),JSON.stringify({injected_monotonic_advance_ms:121000,offer_wall_clock_unchanged:true,fresh_authenticated_device_discovery:true,pair_requests:pairRequests,connected:true},null,2)+'\n');
  await chat('Retained compatibility chat','Native first turn','Compatibility follow-up response','first');
  await boundary('frozen native ABI landscape/sidebar boundary');
  await page.setViewportSize({width:951,height:669});await intent('regular_landscape');
  await page.waitForFunction(()=>window.__thinPresentation?.sidebar_available&&window.__thinPresentation?.sidebar_open);
  await page.locator('[data-mobile-test="agents-view"]').waitFor({state:'visible'});
  await page.screenshot({path:resolvePath(evidence,'integrated-landscape-content.png')});
  await intent('toggle_sidebar');
  await page.locator('[data-mobile-test="agents-view"]').waitFor({state:'hidden'});
  await page.setViewportSize({width:466,height:678});await intent('compact_width');
  await page.waitForFunction(()=>window.__thinPresentation?.sidebar_available===false);
  const paired=await identity(),secretCount=secrets.size;
  if(paired.length!==1||pairRequests!==1)throw Error();
  await page.screenshot({path:resolvePath(initial.evidence,'first-client-chat.png')});
  await intent('home');await page.bringToFront();
  await boundary('authenticated transport loss without re-pair');
  const dropped=new Promise(resolve=>droppedUpgrade=resolve);
  const fresh=new Promise(resolve=>rediscovered=resolve);
  rejectNextUpgrade=true;
  if(sockets.size===0)throw Error();
  for(const socket of sockets)socket.destroy();
  await dropped;
  await Promise.race([fresh,page.locator('[data-mobile-test="connection-banner-repair"]').waitFor({state:'visible'}).then(()=>{throw Error('Transient authenticated transport loss demanded re-pair');})]);
  await loaded(first.release);
  if(pairRequests!==1||JSON.stringify(await identity())!==JSON.stringify(paired))throw Error();
  await writeFile(resolvePath(evidence,'authenticated-transport-recovery.json'),JSON.stringify({network_drops:networkDrops,rediscovered:true,retained_pairing:true,pair_requests:pairRequests},null,2)+'\n');
  await boundary('initial core pair and conversation complete');emit({event:'paired'});
  const managed=initial.service?await managedProbe(await receive()):null;
  const stopped=await receive();if(stopped.event!=='host-stopped')throw Error();
  await page.bringToFront();
  await boundary('actual host downtime and automatic reconnect');
  const offline=new Promise(resolve=>{offlineObserved=resolve;if(offlineCount>0)resolve();});
  for(const socket of sockets)socket.destroy();await intent('foreground');await offline;
  emit({event:'offline',failed_discovery_requests:offlineCount});
  if((await receive()).event!=='restart-publication')throw Error();
  failNextHostPublication=true;emit({event:'publication-fault-armed'});
  const update=await receive();targets.set('native-host.test',update.address);
  await boundary('fresh source/asset upgrade');
  if(managed)await managed.reload(update.release,'upgraded');
  await page.bringToFront();await page.waitForFunction(()=>document.visibilityState==='visible');
  await boundary('replacement client bootstrap');
  await loaded(update.release);
  await chat(update.title,'Native after source upgrade','Compatibility post-upgrade response','replacement');
  if(JSON.stringify(await identity())!==JSON.stringify(paired)||pairRequests!==1||secrets.size!==secretCount)throw Error();
  await page.screenshot({path:resolvePath(initial.evidence,'upgraded-client-chat.png')});
  await boundary('retained rename draft fences client reload');
  await page.evaluate(()=>{
    const original=window.__tydeLoader.nativeReloadAllowed.bind(window.__tydeLoader);
    window.__tydeLoader.nativeReloadAllowed=(...args)=>{
      window.__thinReloadSafety={allowed:args[0],pairingSafe:args[3]};return original(...args);
    };
  });
  const renameDocuments=documents;
  await intent('rename_agent');
  const rename=page.locator('[data-mobile-test="chat-rename-input"]');
  await rename.fill('Retained rename edit');
  await page.evaluate(()=>window.__thinRenameNode=document.querySelector('[data-mobile-test="chat-rename-input"]'));
  await intent('home');
  await page.waitForFunction(()=>window.__thinReloadSafety?.allowed===false&&window.__thinReloadSafety?.pairingSafe===false);
  await intent('agents');
  await rename.waitFor({state:'visible'});
  if(documents!==renameDocuments||await rename.inputValue()!=='Retained rename edit'||
      !await page.evaluate(()=>window.__thinRenameNode===document.querySelector('[data-mobile-test="chat-rename-input"]')))throw Error('Rename edit lost its retained document');
  await page.locator('[data-mobile-test="chat-rename-cancel"]').click();
  await intent('home');
  await page.waitForFunction(()=>window.__thinReloadSafety?.allowed===true&&window.__thinReloadSafety?.pairingSafe===true);
  await writeFile(resolvePath(evidence,'retained-rename-safety-proof.json'),JSON.stringify({same_document:true,same_input_node:true,
    edited_value_retained:true,home_emits_reload_and_pairing_unsafe:true,cancel_releases_safety:true,
    limit:'Actual source-built client DOM and canonical loader safety API; not Swift runtime.'},null,2)+'\n');
  await boundary('retained sessions navigation');
  await intent('sessions');await page.locator('[data-mobile-test="sessions-view"]').waitFor({state:'visible'});
  await page.locator('[data-mobile-test="session-row"]').first().waitFor({state:'visible'});
  await page.screenshot({path:resolvePath(initial.evidence,'retained-sessions.png')});await intent('home');
  console.error('NATIVE_BOUNDARY source upgrade and fresh reply observed');
  emit({event:'upgraded',documents,pair_requests:pairRequests});
  if(initial.service)for(const phase of ['after','held','failed_registration']){
    const arm=await receive();if(arm.event!=='arm-managed-publication'||arm.phase!==phase)throw Error();
    holdNextHostPublication=true;emit({event:'managed-publication-armed'});
    const resumed=await receive();
    if(resumed.event!=='resume-managed-publication'||!releaseHostPublication)throw Error();
    targets.set('lifecycle-host.test',resumed.address);
    releaseHostPublication();releaseHostPublication=null;
    emit({event:'managed-publication-released'});
    if((await receive()).event!=='managed-offer-registered')throw Error('Missing completed current offer registration');
    await managedProbe(resumed);
  }
  await page.bringToFront();await page.waitForFunction(()=>document.visibilityState==='visible');
  await loaded(update.release);
  const second=await receive();
  if(second.event==='finish'){emit({event:'done'});}
  else {
    targets.set('second-host.test',second.address);
    await boundary('unsent draft refuses pairing before redemption');
    await intent('agents');await page.waitForFunction(()=>window.__thinPresentation?.route==='chat');
    await page.locator('[data-mobile-test="chat-input"]').fill('Unsent compatibility draft');
    await intent('hosts');await page.getByRole('button',{name:'Pair another host',exact:true}).click();
    await page.getByRole('button',{name:'Paste link',exact:true}).click();
    await page.getByRole('textbox',{name:'Pairing link',exact:true}).fill(second.qr_uri);
    await page.getByRole('button',{name:'Continue',exact:true}).click();
    await page.locator('.pairing-error').filter({hasText:'finish or save'}).waitFor({state:'visible'});
    if(pairRequests!==1 || (await identity()).length!==1)throw Error('Unsafe pairing consumed an offer');
    await page.getByRole('button',{name:'Cancel',exact:true}).click();await intent('agents');
    if(await page.locator('[data-mobile-test="chat-input"]').inputValue()!=='Unsent compatibility draft')throw Error('Refused pairing lost the draft');
    await page.locator('[data-mobile-test="chat-input"]').fill('');
    await boundary('cancelled pairing refuses late publisher completion');
    const documentsBeforeCancel=documents;
    await page.evaluate(()=>{
      const prepare=window.__tydeLoader.nativePreparePairing.bind(window.__tydeLoader);
      window.__tydeLoader.nativePreparePairing=async(...args)=>{
        const outcome=await prepare(...args);await window.__thinPairingSettled(outcome);return outcome;
      };
      const fetch=window.fetch,reload=window.__tydeLoader.nativeReloadAllowed;
      window.fetch=(input,options)=>{
        const url=new URL(input instanceof Request?input.url:String(input),location.href);
        if(url.origin!==location.origin||url.pathname!=='/tyde/manifest.json')return fetch.call(window,input,options);
        const stack=new Error().stack;
        if(typeof stack!=='string')throw Error('Manifest caller evidence unavailable');
        const pairing=stack.split('\n').some(line=>line.includes('preparePairing')&&line.includes('/native-bootstrap.js:'));
        const headers=new Headers(options?.headers??(input instanceof Request?input.headers:undefined));
        headers.set('x-tyde-native-pairing-test',pairing?'1':'0');
        return fetch.call(window,input,{...options,headers});
      };
      window.__tydeLoader.nativeReloadAllowed=(...args)=>{
        const result=reload(...args);
        if(window.__thinCancelArmed&&args[1]===false){window.__thinCancelArmed=false;void window.__thinPairingCancellationObserved();}
        return result;
      };
      window.__thinRestorePairingProbe=()=>{
        window.fetch=fetch;window.__tydeLoader.nativeReloadAllowed=reload;
        delete window.__thinCancelArmed;delete window.__thinRestorePairingProbe;
      };
    });
    await intent('hosts');await page.getByRole('button',{name:'Pair another host',exact:true}).click();
    await page.getByRole('button',{name:'Paste link',exact:true}).click();
    await page.getByRole('textbox',{name:'Pairing link',exact:true}).fill(second.qr_uri);
    const publisherHeld=new Promise(resolve=>pairingManifestArrived=resolve);
    const cancelledOperation=new Promise(resolve=>pairingOutcome=resolve);
    holdPairingManifest=true;
    const unrelatedRead=page.evaluate(async()=>{
      const response=await fetch('/tyde/manifest.json',{cache:'no-store'});return response.ok;
    });
    const firstPublication=await Promise.race([
      unrelatedRead.then(ok=>({read:true,ok})),publisherHeld.then(held=>({read:false,...held})),
    ]);
    if(!firstPublication.read){
      releasePairingManifest();await unrelatedRead;
      await writeFile(resolvePath(evidence,'cancelled-publisher-identity.json'),JSON.stringify({unrelated_read_consumed_hold:true,
        held_pairing:firstPublication.pairing,real_http_response_released:true},null,2)+'\n');
      throw Error('Unrelated publication read consumed pairing hold');
    }
    if(!firstPublication.ok)throw Error('Unrelated publication read failed');
    await page.getByRole('button',{name:'Continue',exact:true}).click();
    const held=await Promise.race([publisherHeld,cancelledOperation.then(()=>{throw Error('Pairing finished before its publication hold');})]);
    if(!held.pairing)throw Error('Publication hold did not belong to pairing');
    const cancelObserved=new Promise(resolve=>pairingCancellationObserved=resolve);
    await page.evaluate(()=>{window.__thinCancelArmed=true;});
    await page.getByRole('button',{name:'Cancel',exact:true}).click();
    await cancelObserved;
    await writeFile(resolvePath(evidence,'cancelled-publisher-identity.json'),JSON.stringify({unrelated_read_consumed_hold:false,
      unrelated_read_completed:true,held_pairing:true,cancellation_owner_acknowledged:true},null,2)+'\n');
    await intent('agents');await page.waitForFunction(()=>window.__thinPresentation?.route==='chat');
    await page.locator('[data-mobile-test="chat-input"]').fill('Draft after cancelled pairing');
    releasePairingManifest();
    const cancellation=await cancelledOperation;
    const cancelledObservation={
      outcome_cancelled:cancellation.status==='unavailable'&&cancellation.reason==='cancelled',
      same_document:documents===documentsBeforeCancel,
      no_extra_pair_request:pairRequests===1,
      draft_retained:await page.locator('[data-mobile-test="chat-input"]').inputValue()==='Draft after cancelled pairing',
      pending_pair_absent:await page.evaluate(()=>sessionStorage.getItem('tyde.native.pending-pair.v1'))===null,
    };
    await writeFile(resolvePath(evidence,'cancelled-pairing-observation.json'),JSON.stringify(cancelledObservation,null,2)+'\n');
    for(const [invariant,passed] of Object.entries(cancelledObservation))if(!passed)throw Error(`Cancelled pairing invariant failed: ${invariant}`);
    await writeFile(resolvePath(evidence,'cancelled-pairing-publication-proof.json'),JSON.stringify({real_manifest_response_held:true,
      explicit_cancel_before_release:true,settled_outcome:'cancelled',same_document:true,draft_retained:true,pair_requests:pairRequests,
      synchronization:'actual HTTP arrival/release and operation-completion event; no sleeps/time thresholds'},null,2)+'\n');
    await page.evaluate(()=>window.__thinRestorePairingProbe());
    await page.locator('[data-mobile-test="chat-input"]').fill('');
    await boundary('different-wire host switch');
    await intent('hosts');await page.getByRole('button',{name:'Pair another host',exact:true}).click();
    await page.getByRole('button',{name:'Paste link',exact:true}).click();
    await page.getByRole('textbox',{name:'Pairing link',exact:true}).fill(second.qr_uri);
    await page.getByRole('button',{name:'Continue',exact:true}).click();
    await page.getByRole('button',{name:'Pair',exact:true}).click();await loaded(second.release);
    if((await identity()).length!==2||pairRequests!==2)throw Error();
    await intent('hosts');await page.locator('.paired-host-card').filter({has:page.getByText('https://native-host.test',{exact:true})}).getByRole('button',{name:/^Open /}).click();
    await loaded(update.release);
    const final=await identity();if(!final.some(row=>JSON.stringify(row)===JSON.stringify(paired[0]))||pairRequests!==2)throw Error();
    await page.screenshot({path:resolvePath(initial.evidence,'host-switch-retained.png')});
    emit({event:'need-third-offer'});const third=await receive();
    await boundary('third pairing with saved inactive records');
    await intent('hosts');await page.getByRole('button',{name:'Pair another host',exact:true}).click();
    await page.getByRole('button',{name:'Paste link',exact:true}).click();
    await page.getByRole('textbox',{name:'Pairing link',exact:true}).fill(third.qr_uri);
    await page.getByRole('button',{name:'Continue',exact:true}).click();
    await page.getByRole('button',{name:'Pair',exact:true}).click();await loaded(second.release);
    if((await identity()).length!==3||pairRequests!==3)throw Error('Inactive records prevented third pairing completion');
    await intent('hosts');await page.locator('.paired-host-card').filter({has:page.getByText('https://native-host.test',{exact:true})}).getByRole('button',{name:/^Open /}).click();
    await loaded(update.release);
    let three=await identity();if(!final.every(row=>three.some(saved=>JSON.stringify(saved)===JSON.stringify(row))))throw Error('Existing identity changed');
    await writeFile(resolvePath(evidence,'pairing-admission-proof.json'),JSON.stringify({unsafe_draft_refused_before_redemption:true,cancel_retained_draft:true,accepted_retained_chat:true,records:three.length,pair_requests:pairRequests,third_pairing_with_inactive_record:true},null,2)+'\n');
    await boundary('bootstrap-owned inactive pairing removal');
    await intent('hosts');
    const inactive=three.find(row=>row[0]!==paired[0][0]),beforeForgetSecrets=new Map(secrets);
    const row=page.locator('.paired-host-card').filter({hasText:inactive[3]});
    await row.locator('[data-mobile-test="paired-host-forget"]').click();
    const removalDocument=page.waitForEvent('domcontentloaded');
    await row.locator('[data-mobile-test="paired-host-forget-confirm"]').click();
    await removalDocument;await loaded(update.release);
    const afterForget=await identity();
    // Direct records have pskKeychainKeyId=None and one device token, unlike
    // managed records. Require exactly that key removed and all other values intact.
    if(afterForget.length!==2||afterForget.some(row=>row[0]===inactive[0])||!afterForget.some(row=>JSON.stringify(row)===JSON.stringify(paired[0]))||
        secrets.size!==beforeForgetSecrets.size-1||[...beforeForgetSecrets].some(([key,value])=>key===inactive[2]?secrets.has(key):secrets.get(key)!==value)||
        pairRequests!==3)throw Error('Cross-host removal lost record/secret custody');
    three=afterForget;
    await writeFile(resolvePath(evidence,'inactive-pairing-removal-proof.json'),JSON.stringify({explicit_two_step_delete:true,bootstrap_owned_cleanup:true,
      selected_identity_retained:true,inactive_credentials_removed:true,records_before:3,records_after:2,pair_requests:pairRequests,
      limit:'Actual full DOM/IndexedDB flow with scoped platform seam. Native Keychain execution is not claimed.'},null,2)+'\n');
    const rejections=[];
    for(const fault of ['unavailable','blocked','abi','integrity']){
      stage=`explicit ${fault} rejection`;publisherFault=fault;
      await page.evaluate(()=>caches.delete('tyde-bundle-v1'));
      await page.reload();
      await page.locator('#view-error').waitFor({state:'visible'});
      if(await page.evaluate(()=>window.__tydeLoader?.bootVersion())!==null||pairRequests!==3)throw Error();
      await page.screenshot({path:resolvePath(initial.evidence,`rejected-${fault}.png`)});
      rejections.push({fault,executed_client:false,extra_pairing:false});
    }
    publisherFault=null;
    await page.getByRole('button',{name:'Hosts',exact:true}).click();
    await page.getByRole('button').filter({hasText:'https://native-host.test'}).click();
    await loaded(update.release);
    if(JSON.stringify(await identity())!==JSON.stringify(three)||pairRequests!==3)throw Error();
    await writeFile(resolvePath(initial.evidence,'explicit-rejection-proof.json'),JSON.stringify({rejections,explicit_recovery:true},null,2)+'\n');
    emit({event:'switched',documents,pair_requests:pairRequests,device_id:paired[0][1]});
    if((await receive()).event!=='revoked')throw Error();
    await boundary('revoked authority on actual reconnect');
    // This guards reconnect admission, not immediate open-session teardown.
    for(const socket of sockets)socket.destroy();
    await page.locator('[data-mobile-test="connection-banner-repair"]').waitFor({state:'visible'});
    if(await page.locator('[data-mobile-test="connection-banner-update-required"]').isVisible()||pairRequests!==3)throw Error('Revocation became compatibility failure');
    await page.screenshot({path:resolvePath(evidence,'revoked-pairing-repair.png')});
    await writeFile(resolvePath(evidence,'revoked-pairing-proof.json'),JSON.stringify({real_owner_revocation:true,existing_repair_action:true,version_error:false,extra_pairing:false},null,2)+'\n');
    emit({event:'revocation-verified'});
    if(managed){
      if((await receive()).event!=='managed-revoked')throw Error();
      await managed.verifyRevoked();emit({event:'managed-revocation-verified'});
    }
    await receive();emit({event:'done'});
  }
  await writeFile(resolvePath(initial.evidence,'shell-runtime-proof.json'),JSON.stringify({shell_archive_sha256:custody.archive_sha256,native_contract_sha256:custody.native_contract_sha256,unchanged_native_consumer_enforced:true,shell_release:metadata.release,
    current_client_release:entryFor(initial.current_bundle),clients:manifests.map(m=>({release:m.servedClientRelease,protocol:m.versions[m.servedClientRelease].protocolVersion,entry:m.versions[m.servedClientRelease].entry})),
    same_shell:true,retained_pairing_before_explicit_owner_revocation:true,pair_requests:pairRequests,documents,limit:'Linux Chrome executes actual frozen native bridge and actual published-layout source-built Wasm. Not Swift, iOS Keychain, real OAuth, production TLS or physical fold proof.'},null,2)+'\n');
}catch(error){
  const message=typeof error?.message==='string'?error.message:'';
  const failure={stage,
    error_kind:['Error','TypeError','TimeoutError'].includes(error?.name)?error.name:'Other',
    api:message.match(/^(?:page|locator)\.[A-Za-z]+:/)?.[0]??null,
    explicit_bootstrap_failure:message==='Explicit bootstrap failure',
    io_error:['ENOENT','EACCES','ENOSPC','EPIPE'].includes(error?.code)?error.code:null};
  if(evidence)await writeFile(resolvePath(evidence,'safe-driver-error.json'),JSON.stringify(failure,null,2)+'\n');
  console.error(`Native client upgrade failed at ${stage}`);
  if(page&&evidence&&!page.isClosed()){
    await page.locator('textarea,input,#scan-video').evaluateAll(nodes=>nodes.forEach(node=>node.style.visibility='hidden'));
    await page.screenshot({path:resolvePath(evidence,'boundary-failure-redacted.png')});
    const state=await page.evaluate(()=>({loaderError:document.getElementById('error-message')?.textContent,
      secure:window.isSecureContext,worker:!!navigator.serviceWorker?.controller,client:window.__tydeLoader?.bootVersion()}));
    await writeFile(resolvePath(evidence,'boundary-failure.json'),JSON.stringify({stage,...state},null,2)+'\n');
  }
  process.exitCode=1;
}
finally{
  closing=true;
  if(evidence)await writeFile(resolvePath(evidence,'driver-teardown.json'),JSON.stringify({phase:'cancelling_owned_requests',requests:requests.size,transports:transports.size})+'\n');
  for(const request of requests)request.destroy();
  for(const socket of transports)socket.destroy();
  for(const socket of sockets)socket.destroy();
  if(browser)await browser.close();
  if(evidence)await writeFile(resolvePath(evidence,'driver-teardown.json'),JSON.stringify({phase:'browser_closed',requests:requests.size,transports:transports.size})+'\n');
  for(const socket of transports)socket.destroy();
  for(const socket of sockets)socket.destroy();
  for(const server of [proxy,publisher])if(server){await new Promise(resolve=>{server.close(resolve);server.closeAllConnections();});}
  process.stdin.destroy();await rm(temporary,{recursive:true,force:true});
  if(evidence)await writeFile(resolvePath(evidence,'driver-teardown.json'),JSON.stringify({phase:'owned_resources_closed',requests:requests.size,transports:transports.size})+'\n');
}
