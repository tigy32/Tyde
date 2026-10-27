#!/usr/bin/env python3
"""Hold a real control write until the addressed native Claude turn ends."""
import asyncio,json,os,sys
from pathlib import Path
async def main():
    real=os.environ['TYDE_CLAUDE_CONTROL_REAL']
    if '--input-format' not in sys.argv[1:]: os.execv(real,[real,*sys.argv[1:]])
    child=await asyncio.create_subprocess_exec(real,*sys.argv[1:],stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,limit=32*1024*1024)
    pending=None; holding=False; old_terminal=False; buffer=[]; tool_open=False
    proof=Path(os.environ['TYDE_CLAUDE_END_PROOF'])
    state={'held_controls':0,'native_old_terminals':0,'forwarded_after_native_terminal':0,'native_new_terminals':0,'fabricated_frames':0,'foreground_tool_responses':0,'frontier_interrupts':0,'fresh_controls_after_idle':0}
    def save(): proof.write_text(json.dumps(state))
    async def inputs():
        nonlocal pending,holding
        source=asyncio.StreamReader(limit=32*1024*1024)
        await asyncio.get_running_loop().connect_read_pipe(lambda:asyncio.StreamReaderProtocol(source),sys.stdin.buffer)
        while line:=await source.readline():
            frame=json.loads(line); content=frame.get('message',{}).get('content','')
            if isinstance(content,list): content=''.join(x.get('text','') for x in content)
            if frame.get('type')=='control_request' and frame.get('request',{}).get('subtype')=='interrupt' and tool_open:
                state['frontier_interrupts']+=1;save()
            target=(frame.get('type')=='user' and isinstance(content,str) and content.startswith('The word is ') and 'currently executing' not in content)
            if target and not tool_open:
                state['fresh_controls_after_idle']+=1;save()
            if (tool_open and not state['held_controls'] and frame.get('type')=='user' and isinstance(content,str) and content.startswith('The word is ') and 'currently executing' not in content):
                pending=line;holding=True;state['held_controls']+=1;save();continue
            child.stdin.write(line);await child.stdin.drain()
        child.stdin.close()
    async def outputs():
        nonlocal pending,holding,old_terminal,tool_open
        while line:=await child.stdout.readline():
            frame=json.loads(line)
            if frame.get('type') in ('assistant','stream_event') and b'control-release.fifo' in line and not tool_open:
                tool_open=True;state['foreground_tool_responses']+=1;save()
            if frame.get('type')=='result': tool_open=False
            if holding:
                buffer.append(line)
                if frame.get('type')=='result':
                    if not old_terminal:
                        old_terminal=True;state['native_old_terminals']+=1
                        assert pending is not None
                        child.stdin.write(pending);await child.stdin.drain();pending=None
                        state['forwarded_after_native_terminal']+=1;save()
                    else:
                        state['native_new_terminals']+=1;save()
                        for queued in buffer: sys.stdout.buffer.write(queued)
                        sys.stdout.buffer.flush();buffer.clear();holding=False
            else:
                sys.stdout.buffer.write(line);sys.stdout.buffer.flush()
    tasks=[asyncio.create_task(op()) for op in (inputs,outputs)]
    try:
        done,_=await asyncio.wait(tasks,return_when=asyncio.FIRST_COMPLETED)
        for task in done:task.result()
    finally:
        for task in tasks:task.cancel()
        await asyncio.gather(*tasks,return_exceptions=True)
        if child.returncode is None:child.kill()
        await child.wait()
asyncio.run(main())
