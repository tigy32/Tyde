"""Pin a real Hermes terminal/notification overlap without fabricating events."""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time


def prepare_native_control_policy(home):
    """Configure real native policy, without replacing a provider or its events.

    Apply this identical isolated-home setup to every eligible backend run.
    Hermes consumes its native plugin; other providers execute the deliberately
    failing command. The shared scenario requires one actual failed settlement.
    """
    home = Path(home)
    plugin = home / 'plugins' / 'tyde-control-policy'
    plugin.mkdir(parents=True)
    (home / 'config.yaml').write_text('plugins:\n  enabled: [tyde-control-policy]\n')
    (plugin / 'plugin.yaml').write_text(
        'name: tyde-control-policy\nversion: "1.0.0"\n'
        'description: Deny the isolated control-policy probe\nhooks: [pre_tool_call]\n')
    (plugin / '__init__.py').write_text(
        'def before_tool(*, tool_name="", args=None, **kwargs):\n'
        '    if isinstance(args, dict) and "TYDE_NATIVE_POLICY_PROBE" in str(args.get("command", "")):\n'
        '        return {"action": "block", "message": "Control policy probe denied by native plugin"}\n'
        '    return None\n\n'
        'def register(ctx):\n'
        '    ctx.register_hook("pre_tool_call", before_tool)\n')


def native_policy_case():
    import subprocess
    with tempfile.TemporaryDirectory(prefix='tyde-control-native-policy-') as directory:
        home = Path(directory) / 'hermes'
        prepare_native_control_policy(home)
        if sys.argv[3].startswith('real_control_at_native_halt::'):
            with (home / 'config.yaml').open('a') as config:
                config.write('tool_loop_guardrails:\n  hard_stop_enabled: true\n  hard_stop_after:\n    same_tool_failure: 1\n')
        env = dict(os.environ, HERMES_HOME=str(home), TYDE_CONTROL_NATIVE_POLICY_PROOF='1')
        result = subprocess.run([sys.argv[2], '--exact', sys.argv[3], '--ignored', '--nocapture'], env=env)
        return result.returncode


def native_python():
    real_python = os.environ['TYDE_SETTLEMENT_REAL_PYTHON']
    args = sys.argv[1:]
    if (len(args) < 2 or args[0] != '-c'
            or '_tyde_gateway_server._make_agent = _tyde_make_agent' not in args[1]
            or 'from tui_gateway.entry import main' not in args[1]):
        os.execv(real_python, [real_python, *args])
    import socket
    code = args[1]
    baseline = os.environ.get('TYDE_SETTLEMENT_BASELINE_ENTRY')
    if baseline:
        code = Path(baseline).read_text()
    proof_path = Path(os.environ['TYDE_SETTLEMENT_PROOF'])
    state = dict(held=0, claimed=0, claim_attempts=0, released=0, native_errors=0,
                 fabricated_events=0, failed_builds=0, blocked_commits=0, nonobject_provider_calls=0, invalid_argument_commits=0, buffered_steers=0, controlled_halts=0, native_failed_observations=0, source_sha256=hashlib.sha256(code.encode()).hexdigest())
    lock = threading.Lock()
    released = threading.Event()
    armed = False
    held = False
    claim_seen = False
    connection = None

    def save():
        proof_path.write_text(json.dumps(state))

    def emit(byte):
        connection.sendall(byte)

    def controls():
        nonlocal armed
        while command := connection.recv(1):
            with lock:
                if command == b'A':
                    armed = True
                    emit(b'A')
                elif command == b'R':
                    state['released'] += 1
                    save()
                    released.set()
                else:
                    raise RuntimeError('unknown native settlement command')

    def profile(frame, event, result):
        nonlocal connection, armed, held, claim_seen
        name = frame.f_code.co_name
        if event == 'call' and name in ('_tyde_run_conversation', '_tyde_run_after_agent_ready', '_run_after_agent_ready') and connection is None:
            threading.excepthook = worker_error
            connection = socket.socket(socket.AF_UNIX)
            connection.connect(os.environ['TYDE_PENDING_SETTLEMENT_SOCKET'])
            emit(b'P')
            threading.Thread(target=controls, daemon=True).start()
            with lock:
                save()
        if event == 'call' and name == '_tyde_execute':
            for call in frame.f_locals['assistant_message'].tool_calls:
                arguments = json.loads(call.function.arguments)
                if not isinstance(arguments, dict):
                    with lock:
                        state['nonobject_provider_calls'] += 1
                        save()
        if event == 'return' and name == '_append_invalid_arguments_result' and result is True:
            with lock:
                state['invalid_argument_commits'] += 1
                save()
        if event == 'return' and name == '_commit_tool_result' and result is not None and frame.f_locals.get('blocked') is True:
            with lock:
                state['blocked_commits'] += 1
                save()
        if event == 'return' and name == '_wait_agent_for_prompt' and result is not None:
            with lock:
                state['failed_builds'] += 1
                save()
        if event == 'return' and name == '_tyde_make_agent' and result is not None:
            config = result._tool_guardrails.config
            with lock:
                state['guardrail_hard_stop_enabled'] = config.hard_stop_enabled
                state['guardrail_failure_halt_after'] = config.same_tool_failure_halt_after
                save()
        if event == 'return' and name == '_append_guardrail_observation' and frame.f_locals['failed']:
            with lock:
                state['native_failed_observations'] += 1
                save()
        if event == 'return' and name == 'steer' and frame.f_code.co_filename.endswith('/agent/interrupt_control.py') and result is True:
            with lock:
                state['buffered_steers'] += 1
                save()
        if event == 'call' and name == '_toolguard_controlled_halt_response':
            with lock:
                state['controlled_halts'] += 1
                save()
        if event == 'call' and name == '_tyde_settled_info':
            with lock:
                pin = armed
                if pin:
                    armed = False
                    held = True
                    claim_seen = False
                    released.clear()
                    state['held'] += 1
                    state['held_native_running'] = bool(frame.f_locals['session'].get('running'))
                    state['held_terminal_present'] = bool(frame.f_globals['_tyde_pending_terminals'].get(frame.f_locals['sid']))
                    save()
                    emit(b'S')
            if pin:
                released.wait()
                with lock:
                    held = False
        elif event == 'return' and name in ('_notif_claim_turn', '_tyde_notif_claim_turn'):
            with lock:
                if held and not claim_seen:
                    claim_seen = True
                    state['claim_attempts'] += 1
                    state['claimed'] += int(result is True)
                    save()
                    emit(b'C')

    def worker_error(error):
        with lock:
            state['native_errors'] += 1
            trace = error.exc_traceback
            while trace is not None and trace.tb_next is not None:
                trace = trace.tb_next
            state['error_site'] = trace.tb_frame.f_code.co_name if trace is not None else 'unknown'
            state['error_type'] = error.exc_type.__name__
            save()
            if connection is not None:
                emit(b'E')
        # A genuine unhandled worker failure must fail this owned native
        # process, not leave the conformance client waiting on a dead owner.
        os._exit(70)

    threading.excepthook = worker_error
    threading.setprofile(profile)
    sys.setprofile(profile)
    sys.argv = ['-c', *args[2:]]
    exec(compile(code, '<string>', 'exec'), {'__name__': '__main__'})


async def run():
    parser = argparse.ArgumentParser()
    parser.add_argument('binary')
    parser.add_argument('--python', required=True)
    parser.add_argument('--baseline-entry')
    parser.add_argument('--case', default='real_immediate_turn_controls::hermes')
    args = parser.parse_args()
    os.umask(0o077)
    directory = Path(tempfile.mkdtemp(prefix='tyde-pending-settlement-'))
    path = directory / 'gate.sock'
    producer = asyncio.Future()
    consumer = asyncio.Future()
    connections = []

    async def relay(reader, writer):
        connections.append(writer)
        first = await reader.readexactly(1)
        if first == b'P':
            producer.set_result(writer)
            destination = await consumer
        elif first == b'T':
            consumer.set_result(writer)
            destination = await producer
        else:
            raise RuntimeError('unknown native settlement peer')
        while data := await reader.read(1024):
            destination.write(data)
            await destination.drain()

    server = await asyncio.start_unix_server(relay, path=path)
    wrapper = directory / 'python'
    wrapper.write_text('#!' + str(Path(args.python).absolute()) + '\n'
                       'import runpy\n'
                       'runpy.run_path(' + repr(str(Path(__file__).resolve())) + ')["native_python"]()\n')
    wrapper.chmod(0o700)
    env = dict(os.environ, HERMES_PYTHON=str(wrapper),
               TYDE_SETTLEMENT_REAL_PYTHON=str(Path(args.python).absolute()),
               TYDE_PENDING_SETTLEMENT_SOCKET=str(path),
               TYDE_SETTLEMENT_PROOF=str(directory / 'proof.json'))
    if args.baseline_entry:
        env['TYDE_SETTLEMENT_BASELINE_ENTRY'] = str(Path(args.baseline_entry).resolve())
    started = time.monotonic()
    with (directory / 'run.log').open('wb') as output:
        test = await asyncio.create_subprocess_exec(
            args.binary, '--exact', args.case,
            '--ignored', '--nocapture', env=env, stdout=output, stderr=asyncio.subprocess.STDOUT)
        try:
            rc = await asyncio.wait_for(test.wait(), 600)
        except asyncio.TimeoutError:
            test.kill()
            await test.wait()
            rc = 124
    receipt = dict(test_exit=rc, duration_seconds=round(time.monotonic() - started, 2))
    (directory / 'result.json').write_text(json.dumps(receipt))
    proof = json.loads((directory / 'proof.json').read_text()) if (directory / 'proof.json').exists() else None
    if args.case == 'real_immediate_turn_controls::hermes' and rc == 0 and (proof is None or any(proof[key] != value for key, value in
                     dict(held=1, claimed=0, claim_attempts=1, released=1, native_errors=0, fabricated_events=0, blocked_commits=1).items())):
        raise RuntimeError('successful case did not exercise the genuine native settlement overlap')
    if args.case == 'real_control_initialization_failure::hermes' and rc == 0:
        if proof is None or proof['failed_builds'] != 2:
            raise RuntimeError('negative case must exercise both genuine native build failures')
    if args.case == 'real_control_at_native_halt::hermes':
        if (proof is None or proof.get('guardrail_hard_stop_enabled') is not True
                or proof.get('guardrail_failure_halt_after') != 1):
            raise RuntimeError('native controlled-halt configuration was not actually applied')
        if proof['buffered_steers'] and not proof['controlled_halts']:
            raise RuntimeError('buffered-control baseline did not reach a genuine controlled halt')
    for connection in connections:
        connection.close()
    server.close()
    await server.wait_closed()
    print('Private native settlement evidence:', directory)
    print('Result:', json.dumps(receipt))
    print('Native proof:', json.dumps(proof))
    return rc


if __name__ == '__main__':
    if sys.argv[1:2] == ['--native-policy-profile']:
        sys.exit(native_policy_case())
    sys.exit(asyncio.run(run()))
