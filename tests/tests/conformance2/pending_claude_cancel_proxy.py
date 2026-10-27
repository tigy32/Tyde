#!/usr/bin/env python3
"""Pin real adapter admission and native cancellation without fabricating frames."""
import asyncio,json,os,sys
from pathlib import Path
async def main():
    real=os.environ['TYDE_CLAUDE_CONTROL_REAL']
    if '--input-format' not in sys.argv[1:]: os.execv(real,[real,*sys.argv[1:]])
    child=await asyncio.create_subprocess_exec(real,*sys.argv[1:],stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,limit=32*1024*1024)
    proof=Path(os.environ['TYDE_CLAUDE_CANCEL_PROOF'])
    state=dict(idle_interrupts=0,native_idle_interrupt_receipts=0,cancelled_prompt_after_receipt=0,cancelled_prompt_after_admission_release=0,cancelled_tools_released=0,fabricated_frames=0)
    open_turn=False; acknowledgements={}; cancelled_target=False; release_tasks=[]
    def save(): proof.write_text(json.dumps(state))
    save()
    async def release_actual_command(path):
        def write():
            with open(path,'w') as fifo: fifo.write('release\n')
        await asyncio.to_thread(write)
        state['cancelled_tools_released']+=1;save()
    async def inputs():
        nonlocal open_turn,cancelled_target
        source=asyncio.StreamReader(limit=32*1024*1024)
        await asyncio.get_running_loop().connect_read_pipe(lambda:asyncio.StreamReaderProtocol(source),sys.stdin.buffer)
        idle_cancel=False
        while line:=await source.readline():
            frame=json.loads(line);waiting=None
            if frame.get('type')=='control_request' and frame.get('request',{}).get('subtype')=='interrupt' and not open_turn:
                state['idle_interrupts']+=1;save()
                waiting=asyncio.get_running_loop().create_future()
                acknowledgements[frame['request_id']]=waiting
                idle_cancel=True
            if frame.get('type')=='user':
                content=frame.get('message',{}).get('content',[])
                text=content if isinstance(content,str) else ''.join(item.get('text','') for item in content)
                if idle_cancel and 'control-release.fifo' in text:
                    cancelled_target=True;state['cancelled_prompt_after_receipt']+=1;save()
                admission=json.loads(Path(os.environ['TYDE_CLAUDE_CANCEL_GATE_PROOF']).read_text())
                if admission['held']==1 and admission['released']==1 and 'control-release.fifo' in text:
                    cancelled_target=True;state['cancelled_prompt_after_admission_release']+=1;save()
                open_turn=True;idle_cancel=False
            child.stdin.write(line);await child.stdin.drain()
            if waiting is not None:
                await waiting
                state['native_idle_interrupt_receipts']+=1;save()
        child.stdin.close()
    async def outputs():
        nonlocal open_turn,cancelled_target
        released=False
        while line:=await child.stdout.readline():
            frame=json.loads(line)
            if frame.get('type')=='control_response':
                request=frame.get('request_id') or frame.get('response',{}).get('request_id')
                waiter=acknowledgements.pop(request,None)
                if waiter is not None: waiter.set_result(None)
            if cancelled_target and not released and frame.get('type')=='assistant':
                for item in frame.get('message',{}).get('content',[]):
                    if item.get('type')=='tool_use' and item.get('name')=='Bash':
                        import re
                        match=re.search(r"open\('([^']*control-release[.]fifo)'\)",item.get('input',{}).get('command',''))
                        if match:
                            released=True;release_tasks.append(asyncio.create_task(release_actual_command(match[1])))
            if frame.get('type') in ('assistant','stream_event'): open_turn=True
            if frame.get('type')=='result': open_turn=False;cancelled_target=False
            sys.stdout.buffer.write(line);sys.stdout.buffer.flush()
    tasks=[asyncio.create_task(op()) for op in (inputs,outputs)]
    try:
        done,_=await asyncio.wait(tasks,return_when=asyncio.FIRST_COMPLETED)
        for task in done:task.result()
    finally:
        for task in tasks+release_tasks:task.cancel()
        await asyncio.gather(*tasks,*release_tasks,return_exceptions=True)
        if child.returncode is None:child.kill()
        await child.wait()

async def run():
    import argparse,tempfile,time
    parser=argparse.ArgumentParser()
    parser.add_argument('--run',required=True)
    parser.add_argument('--case',default='real_immediate_turn_controls::claude')
    args=parser.parse_args()
    os.umask(0o077)
    directory=Path(tempfile.mkdtemp(prefix='tyde-claude-admission-'))
    path=directory/'gate.sock'
    action=None; armed=False; pending=None
    gate_proof=dict(held=0,released=0,fabricated_frames=0)
    def save_gate(): (directory/'gate-proof.json').write_text(json.dumps(gate_proof))
    async def peer(reader,writer):
        nonlocal action,armed,pending
        first=await reader.readexactly(1)
        if first==b'P':
            if armed:
                armed=False;pending=asyncio.get_running_loop().create_future()
                gate_proof['held']+=1;save_gate()
                action.write(b'H');await action.drain()
                await pending
            writer.write(b'R');await writer.drain()
            writer.close();await writer.wait_closed()
        elif first==b'A':
            if action is not None: raise RuntimeError('duplicate admission observer')
            action=writer;armed=True
            writer.write(b'A');await writer.drain()
            while command:=await reader.read(1):
                if command!=b'R' or pending is None: raise RuntimeError('invalid admission release')
                gate_proof['released']+=1;save_gate();pending.set_result(None);pending=None
            writer.close();await writer.wait_closed()
        else: raise RuntimeError('unknown admission observer')
    server=await asyncio.start_unix_server(peer,path=path)
    save_gate()
    env=dict(os.environ,TYDE_CLAUDE_BIN=str(Path(__file__).resolve()),
        TYDE_CLAUDE_PROMPT_GATE=str(path),TYDE_PENDING_CANCELLATION_SOCKET=str(path),
        TYDE_CLAUDE_CANCEL_PROOF=str(directory/'native-proof.json'),
        TYDE_CLAUDE_CANCEL_GATE_PROOF=str(directory/'gate-proof.json'))
    started=__import__('time').monotonic()
    with (directory/'run.log').open('wb') as output:
        test=await asyncio.create_subprocess_exec(args.run,'--exact',args.case,'--ignored','--nocapture',env=env,stdout=output,stderr=asyncio.subprocess.STDOUT)
        try: rc=await asyncio.wait_for(test.wait(),600)
        except asyncio.TimeoutError:
            test.kill();await test.wait();rc=124
    native=json.loads((directory/'native-proof.json').read_text()) if (directory/'native-proof.json').exists() else None
    receipt=dict(test_exit=rc,duration_seconds=round(__import__('time').monotonic()-started,2),gate=gate_proof,native=native)
    (directory/'result.json').write_text(json.dumps(receipt))
    server.close();await server.wait_closed()
    print('Private Claude admission evidence:',directory)
    print('Result:',json.dumps(receipt))
    if rc==0:
        if gate_proof!=dict(held=1,released=1,fabricated_frames=0): raise RuntimeError('real adapter admission boundary was not pinned exactly once')
        if native is None or native['idle_interrupts']!=0 or native['fabricated_frames']!=0 or native['cancelled_prompt_after_admission_release']!=0:
            raise RuntimeError('unwritten reservation sent a stray native interrupt')
    return rc

if sys.argv[1:2]==['--run']:
    sys.exit(asyncio.run(run()))
asyncio.run(main())
