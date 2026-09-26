const LIMIT = 2048, BYTE_LIMIT = 256 * 1024, LIFETIME = 15 * 60 * 1000;
const events = ['pointerdown','pointerup','pointercancel','touchstart','touchend','touchcancel','mousedown','mouseup','click','focusin','focusout','beforeinput','input','compositionstart','compositionupdate','compositionend'];
const contacts = new Set(['pointerdown','pointerup','pointercancel','touchstart','touchend','touchcancel','mousedown','mouseup','click']);
const phases = new Set([...events, ...[...contacts].map(type => type+':post-dispatch'), 'installed','viewport','geometry','clear-before','clear-after','send-handler','guard-busy-or-photos','guard-terminated','guard-empty','guard-missing-stream','guard-missing-host','guard-custody-cap','submission-begin','input-handler-before','input-handler-after','primary-click-handler','primary-guard-terminated','primary-interrupt','admitted-locally','admission-encoding-failed','admission-not-connected','admission-queue-full','admission-connection-closed']);
const roles = ['composer','primary','menu-backdrop','menu','drawer','other','none'];
const actions = ['Send','Queue','Cancel','Stop','Fork + send','Terminated'];
const inputs = ['insertText','insertCompositionText','insertFromComposition','insertReplacementText','insertLineBreak','insertParagraph','deleteContentBackward','deleteContentForward'];
const lifecycle = ['none','idle','thinking','awaiting-user','terminated'];
const buckets = ['0','1-32','33-128','129-512','513+'];
const allowed = (value, list) => list.includes(value) ? value : null;
const number = value => Number.isFinite(value) && value >= 0 ? Math.min(Math.round(value), Number.MAX_SAFE_INTEGER) : null;
const boolean = value => typeof value === 'boolean' ? value : null;
const bucket = length => length === 0 ? '0' : length <= 32 ? '1-32' : length <= 128 ? '33-128' : length <= 512 ? '129-512' : '513+';
const primaryFor = field => field?.closest('.chat-bottom-dock')?.querySelector('[data-mobile-test="chat-send"]');
const role = (element, field, primary) => !element ? 'none' : element === field ? 'composer' : primary?.contains(element) ? 'primary' : element.closest?.('[data-mobile-test="chat-send-menu-backdrop"]') ? 'menu-backdrop' : element.closest?.('[data-mobile-test="chat-send-menu"]') ? 'menu' : element.closest?.('[data-mobile-test="activity-drawer"]') ? 'drawer' : 'other';
const queryEnabled = () => new URLSearchParams(location.search).get('tyde-send-diagnostics') === '1';
const iosStandalone = () => (/^(iPhone|iPad|iPod)$/.test(navigator.platform) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1)) && (navigator.standalone === true || matchMedia('(display-mode: standalone)').matches);
let session;

// Both storage and export go through this schema; neither spreads event/app state.
function sanitize(record) {
    if (!phases.has(record.phase)) return null;
    return {
        phase:record.phase, at:number(record.at), sequence:number(record.sequence),
        trusted:boolean(record.trusted), defaultPrevented:boolean(record.defaultPrevented),
        target:allowed(record.target,roles), focused:allowed(record.focused,roles),
        composing:boolean(record.composing), compositionOpen:boolean(record.compositionOpen),
        inputType:allowed(record.inputType,[...inputs,'other']), pointerType:allowed(record.pointerType,['touch','mouse','pen','other']),
        nativeLength:allowed(record.nativeLength,buckets), reactiveLength:allowed(record.reactiveLength,buckets),
        nativeEqual:boolean(record.nativeEqual), submitting:boolean(record.submitting), loadingPhotos:boolean(record.loadingPhotos),
        terminated:boolean(record.terminated), hasImages:boolean(record.hasImages), disabled:boolean(record.disabled),
        action:allowed(record.action,actions), lifecycle:allowed(record.lifecycle,lifecycle), samePrimary:boolean(record.samePrimary),
        ageMs:number(record.ageMs), centerRole:allowed(record.centerRole,roles), contactRole:allowed(record.contactRole,roles),
        composerHeight:number(record.composerHeight), primaryHeight:number(record.primaryHeight), primaryWidth:number(record.primaryWidth),
    };
}
function createSession() {
    const state = {status:'Recording', started:performance.now(), startedWall:Date.now(), last:0, reader:null, owners:0, surfaces:new Set(), ring:new Array(LIMIT), head:0, count:0, bytes:0, dropped:0, ordinal:0, composition:false, originalPrimary:null, geometry:null, geometryTimer:null, post:[], postTimer:null, expiryTimer:null, detach:null, exportMessage:'', exportGeneration:0};
    session = state;
    const relative = () => state.last = Math.max(state.last, performance.now()-state.started, 0);
    const clearRecords = () => { state.ring.fill(undefined); state.head=0; state.count=0; state.bytes=0; state.dropped=0; };
    const cancelSamples = () => {
        clearTimeout(state.geometryTimer); clearTimeout(state.postTimer);
        state.geometryTimer=null; state.postTimer=null; state.geometry=null; state.post=[]; state.originalPrimary=null;
    };
    const update = () => { for (const render of state.surfaces) render(); };
    const stop = () => { state.detach?.(); state.detach=null; cancelSamples(); state.composition=false; };
    const expire = () => {
        if(state.status==='Expired')return;
        stop(); clearTimeout(state.expiryTimer); state.expiryTimer=null; clearRecords(); state.status='Expired'; state.exportMessage='Unexported capture cleared after 15 minutes.'; state.exportGeneration++; update();
    };
    const fresh = () => {
        if(performance.now()-state.started >= LIFETIME || Date.now()-state.startedWall >= LIFETIME) expire();
        return state.status==='Recording';
    };
    const snapshot = () => {
        fresh();
        const records=[];
        for(let i=0;i<state.count;i++)records.push(sanitize(state.ring[(state.head+i)%LIMIT].record));
        return {schema:3,status:state.status,dropped:state.dropped,records};
    };
    const push = raw => {
        if(!fresh())return;
        const record=sanitize(raw); if(!record)return;
        const size=new TextEncoder().encode(JSON.stringify(record)).length+1;
        if(size>BYTE_LIMIT-1024){state.dropped++;return;}
        while(state.count && (state.count>=LIMIT || state.bytes+size>BYTE_LIMIT-1024)) {
            const old=state.ring[state.head];state.bytes-=old.size;state.ring[state.head]=undefined;state.head=(state.head+1)%LIMIT;state.count--;state.dropped++;
        }
        state.ring[(state.head+state.count)%LIMIT]={record,size};state.count++;state.bytes+=size;
    };
    const read = () => state.reader ? state.reader() : [0,null,false,false,false,0,null,'none'];
    const observe = (phase,event,sequence) => {
        if(!fresh() || !phases.has(phase))return;
        const [length,equal,submitting,loadingPhotos,terminated,images,field,activity]=read();
        const primary=primaryFor(field), document=field?.ownerDocument || window.document;
        push({phase,at:relative(),sequence,trusted:event?.isTrusted,defaultPrevented:event?.defaultPrevented,
            target:event?role(event.target,field,primary):null,focused:role(document.activeElement,field,primary),
            composing:typeof event?.isComposing==='boolean'?event.isComposing:null,compositionOpen:state.composition,
            inputType:event && ['input','beforeinput'].includes(event.type)?allowed(event.inputType,inputs)||'other':null,
            pointerType:event?.type.startsWith('pointer')?allowed(event.pointerType,['touch','mouse','pen'])||'other':null,
            nativeLength:field?bucket(field.value.length):null,reactiveLength:bucket(length),nativeEqual:equal,
            submitting,loadingPhotos,terminated,hasImages:images>0,disabled:primary?.disabled,
            action:primary?.textContent?.trim(),lifecycle:activity,samePrimary:state.originalPrimary?primary===state.originalPrimary:null});
    };
    const scheduleGeometry = (event, sequence) => {
        if(!fresh())return;
        const touch=event?.changedTouches?.[0];
        const x=touch?.clientX ?? event?.clientX, y=touch?.clientY ?? event?.clientY;
        // Only this one transient point is retained, never exported as a coordinate stream.
        state.geometry={at:relative(),sequence,x,y,primary:primaryFor(read()[6])};
        if(state.geometryTimer!==null)return;
        state.geometryTimer=setTimeout(()=>{
            state.geometryTimer=null;const pending=state.geometry;state.geometry=null;
            if(!pending||!fresh())return;
            const field=read()[6], primary=primaryFor(field);if(!field||!primary)return;
            const r=primary.getBoundingClientRect(),f=field.getBoundingClientRect(),document=field.ownerDocument;
            const quantize=value=>Math.min(4096,Math.max(0,Math.round(value/8)*8));
            push({phase:'geometry',at:relative(),sequence:pending.sequence,ageMs:relative()-pending.at,
                centerRole:role(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2),field,primary),
                contactRole:Number.isFinite(pending.x)&&Number.isFinite(pending.y)?role(document.elementFromPoint(pending.x,pending.y),field,primary):null,
                composerHeight:quantize(f.height),primaryHeight:quantize(r.height),primaryWidth:quantize(r.width),samePrimary:primary===pending.primary});
        },0);
    };
    const capture = event => {
        if(!fresh())return;
        const field=read()[6];
        if((event.type.startsWith('composition')||['input','beforeinput'].includes(event.type))&&event.target!==field)return;
        if(event.type==='compositionstart')state.composition=true;
        if(event.type==='compositionend')state.composition=false;
        if(['pointerdown','touchstart'].includes(event.type))state.originalPrimary=primaryFor(field);
        const sequence=++state.ordinal;
        observe(event.type,event,sequence);
        if(contacts.has(event.type)) {
            if(state.post.length<16)state.post.push({event,sequence});else state.dropped++;
            if(state.postTimer===null)state.postTimer=setTimeout(()=>{
                state.postTimer=null;const pending=state.post;state.post=[];
                for(const {event,sequence} of pending)push({phase:event.type+':post-dispatch',at:relative(),sequence,trusted:event.isTrusted,defaultPrevented:event.defaultPrevented});
            },0);
        }
        if(['input','pointerdown','pointerup','touchstart','touchend','click'].includes(event.type))scheduleGeometry(event,sequence);
    };
    const resized = () => {observe('viewport');scheduleGeometry();};
    const attach = () => {
        for(const type of events)document.addEventListener(type,capture,{capture:true,passive:true});
        window.visualViewport?.addEventListener('resize',resized,{passive:true});
        window.visualViewport?.addEventListener('scroll',resized,{passive:true});
        state.detach=()=>{
            for(const type of events)document.removeEventListener(type,capture,true);
            window.visualViewport?.removeEventListener('resize',resized);window.visualViewport?.removeEventListener('scroll',resized);
        };
    };
    const visibility = () => {fresh();};
    document.addEventListener('visibilitychange',visibility,{passive:true});
    const restart = () => {
        stop();clearTimeout(state.expiryTimer);clearRecords();state.started=performance.now();state.startedWall=Date.now();state.last=0;state.ordinal=0;state.status='Recording';state.exportMessage='';state.exportGeneration++;
        state.expiryTimer=setTimeout(expire,LIFETIME);attach();observe('installed');update();
    };
    state.observe=observe;
    state.controls={
        Stop:()=>{fresh();if(state.status==='Recording'){stop();state.status='Stopped';update();}},
        Clear:()=>{fresh();stop();if(state.status!=='Expired')state.status='Stopped';clearRecords();state.exportMessage='Capture stopped and cleared. Start a new capture to record again.';state.exportGeneration++;update();},
        'Start new capture':restart,
        Export:async()=>{
            fresh(); const generation=state.exportGeneration;
            const json=JSON.stringify(snapshot());
            if(new TextEncoder().encode(json).length>BYTE_LIMIT){state.exportMessage='Export unavailable: size limit.';update();return;}
            const file=new File([json],'tyde-send-diagnostics.json',{type:'application/json'});
            try {
                if(navigator.canShare?.({files:[file]})) await navigator.share({files:[file],title:'Tyde Send diagnostics'});
                else {
                    const url=URL.createObjectURL(file),link=document.createElement('a');link.href=url;link.download=file.name;link.hidden=true;document.body.append(link);
                    try {link.click();} finally {link.remove();URL.revokeObjectURL(url);}
                }
                if(session===state&&generation===state.exportGeneration){state.exportMessage='Export requested. Save or share the local JSON file.';update();}
            } catch {
                if(session===state&&generation===state.exportGeneration){state.exportMessage='Export was canceled or unavailable. Capture stays in memory until expiry.';update();}
            }
        },
    };
    state.destroy=()=>{stop();clearTimeout(state.expiryTimer);clearRecords();document.removeEventListener('visibilitychange',visibility);state.reader=null;state.surfaces.clear();state.exportGeneration++;if(session===state){session=undefined;delete window.__TYDE_SEND_DIAGNOSTICS__;}};
    window.__TYDE_SEND_DIAGNOSTICS__=Object.freeze({schema:3,get events(){return snapshot().records;},get dropped(){return snapshot().dropped;}});
    restart();return state;
}
export function configureSendDiagnostics(beta) {
    if(!((beta&&iosStandalone())||queryEnabled()))return ()=>{};
    const state=session||createSession();state.owners++;
    return ()=>{state.owners--;if(state.owners===0)state.destroy();};
}
export function sendDiagnosticsEnabled() {return !!session||queryEnabled();}
export function markSendDiagnostic(phase) {session?.observe(phase);}
export function installSendDiagnostics(reader) {
    const state=session||(queryEnabled()?createSession():null);if(!state)return ()=>{};
    state.reader=reader;state.composition=false;
    return ()=>{if(state.reader===reader){state.reader=null;state.composition=false;if(!state.owners)state.destroy();}};
}
export function installSendDiagnosticsSurface(root, settings) {
    const state=session;
    root.hidden=!state;
    if(!state)return ()=>{};
    if(!settings) {
        const disclosure=document.createElement('details'),summary=document.createElement('summary'),panel=document.createElement('div');
        summary.setAttribute('aria-label','Send diagnostics controls');
        panel.className='send-diagnostics-controls';panel.dataset.mobileTest='send-diagnostics-global-panel';
        disclosure.append(summary,panel);root.append(disclosure);
        const cleanup=installSendDiagnosticsSurface(panel,true);
        const render=()=>{summary.textContent=`Send diagnostics: ${state.status}`;};
        state.surfaces.add(render);render();
        return ()=>{cleanup();state.surfaces.delete(render);root.replaceChildren();};
    }
    const status=document.createElement('span');status.setAttribute('role','status');root.append(status);
    const buttons=[];
    let detail;
    if(settings) {
        const notice=document.createElement('p');notice.textContent='Beta Send diagnostics record no message text or audio. Memory only, 15 minutes. Reload or expiry clears capture. Export saves a private local JSON file; nothing uploads automatically.';root.append(notice);
        for(const name of ['Stop','Clear','Export','Start new capture']) {
            const button=document.createElement('button');button.type='button';button.textContent=name;
            button.addEventListener('click',state.controls[name]);root.append(button);buttons.push(button);
        }
        detail=document.createElement('p');root.append(detail);
    }
    const render=()=>{
        status.textContent=`Beta Send diagnostics: ${state.status}${settings?'':' · Controls in Settings'}`;
        for(const button of buttons)button.disabled=button.textContent==='Stop'?state.status!=='Recording':button.textContent==='Start new capture'?state.status==='Recording':false;
        if(detail)detail.textContent=state.exportMessage;
    };
    state.surfaces.add(render);render();
    return ()=>{state.surfaces.delete(render);for(const button of buttons)button.removeEventListener('click',state.controls[button.textContent]);root.replaceChildren();};
}
