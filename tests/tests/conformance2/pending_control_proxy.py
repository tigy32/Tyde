"""Pin native Codex turn/start delivery; forward only genuine provider bytes."""
import asyncio
import json
import os
from pathlib import Path
import shlex
import shutil
import sys
import tempfile

async def proxy():
    real = os.environ['TYDE_PENDING_REAL_CODEX']
    if 'app-server' not in sys.argv[2:]:
        os.execv(real, [real, *sys.argv[2:]])
    child = await asyncio.create_subprocess_exec(real, *sys.argv[2:],
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        limit=32 * 1024 * 1024)
    reader, writer = await asyncio.open_unix_connection(os.environ['TYDE_PENDING_CONTROL_SOCKET'])
    writer.write(b'P')
    await writer.drain()
    armed = False
    released = asyncio.Event()
    released.set()
    notifications = asyncio.Queue()
    state = {'held': 0, 'released': 0, 'fabricated_events': 0}
    proof = Path(os.environ['TYDE_PENDING_PROOF'])
    def save():
        proof.write_text(json.dumps(state))
    async def controls():
        nonlocal armed
        while command := await reader.read(1):
            if command == b'A':
                armed = True
                writer.write(b'A')
                await writer.drain()
            elif command == b'R':
                state['released'] += 1
                save()
                released.set()
            else:
                raise RuntimeError('unknown control gate command')
    async def inputs():
        source = asyncio.StreamReader(limit=32 * 1024 * 1024)
        await asyncio.get_running_loop().connect_read_pipe(lambda: asyncio.StreamReaderProtocol(source), sys.stdin.buffer)
        while line := await source.readline():
            child.stdin.write(line)
            await child.stdin.drain()
        child.stdin.close()
    async def outputs():
        nonlocal armed
        while line := await child.stdout.readline():
            msg = json.loads(line)
            if 'id' in msg and 'method' not in msg:
                sys.stdout.buffer.write(line)
                sys.stdout.buffer.flush()
                continue
            hold = armed and msg.get('method') == 'turn/started'
            if hold:
                armed = False
                released.clear()
                state['held'] += 1
                save()
                writer.write(b'H')
                await writer.drain()
            await notifications.put((line, hold))
        await notifications.put(None)
    async def dispatch():
        while entry := await notifications.get():
            line, hold = entry
            if hold:
                await released.wait()
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
    tasks = [asyncio.create_task(op()) for op in (controls, inputs, outputs, dispatch)]
    try:
        done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            task.result()
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        writer.close()
        await writer.wait_closed()
        if child.returncode is None:
            child.kill()
        await child.wait()

async def run():
    os.umask(0o077)
    directory = Path(tempfile.mkdtemp(prefix='tyde-pending-control-'))
    path = directory / 'gate.sock'
    proxy_ready = asyncio.Future()
    test_ready = asyncio.Future()
    connections = []
    async def relay(reader, writer):
        connections.append(writer)
        first = await reader.readexactly(1)
        if first == b'P':
            proxy_ready.set_result(writer)
            dest = await test_ready
        elif first == b'A':
            test_ready.set_result(writer)
            dest = await proxy_ready
            dest.write(first)
            await dest.drain()
        else:
            raise RuntimeError('unknown native gate client')
        while chunk := await reader.read(1024):
            dest.write(chunk)
            await dest.drain()
    server = await asyncio.start_unix_server(relay, path)
    real = shutil.which('codex')
    if real is None:
        raise RuntimeError('real Codex is required')
    bin_dir = directory / 'bin'
    bin_dir.mkdir()
    wrapper = bin_dir / 'codex'
    wrapper.write_text('#!/bin/sh\nexec ' + shlex.quote(sys.executable) + ' ' + shlex.quote(str(Path(__file__).resolve())) + ' --proxy "$@"\n')
    wrapper.chmod(0o700)
    shell = directory / 'bash'
    shell.write_text('#!/bin/sh\nexec /bin/bash --noprofile --norc "$@"\n')
    shell.chmod(0o700)
    env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ['PATH'], SHELL=str(shell),
        TYDE_PENDING_CONTROL_SOCKET=str(path), TYDE_PENDING_REAL_CODEX=real,
        TYDE_PENDING_PROOF=str(directory / 'proof.json'))
    with (directory / 'run.log').open('wb') as output:
        test = await asyncio.create_subprocess_exec(sys.argv[1], '--exact', 'real_immediate_turn_controls::codex',
            '--ignored', '--nocapture', env=env, stdout=output, stderr=asyncio.subprocess.STDOUT)
        try:
            rc = await asyncio.wait_for(test.wait(), 600)
        except asyncio.TimeoutError:
            print('Individual real conformance test hung for ten minutes', flush=True)
            test.kill()
            await test.wait()
            rc = 124
    (directory / 'result.json').write_text(json.dumps({'test_exit': rc}))
    if rc == 0:
        evidence = json.loads((directory / 'proof.json').read_text())
        if evidence != {'held': 3, 'released': 3, 'fabricated_events': 0}:
            raise RuntimeError('successful native control test did not exercise all three genuine start boundaries')
    for connection in connections:
        connection.close()
    server.close()
    await server.wait_closed()
    print('Private native control evidence:', directory)
    if (directory / 'proof.json').exists():
        print('Transport proof:', (directory / 'proof.json').read_text())
    for line in (directory / 'run.log').read_text(errors='replace').splitlines():
        if line.startswith(('test result:', 'an accepted turn must', 'the accepted turn must')):
            print(line)
    return rc

if __name__ == '__main__':
    if sys.argv[1:2] == ['--proxy']:
        asyncio.run(proxy())
    else:
        sys.exit(asyncio.run(run()))
