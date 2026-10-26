#!/usr/bin/env python3
"""Read existing Tyde tracing files; emit only allowlisted diagnostic categories."""
from fnmatch import fnmatchcase
import json
from pathlib import Path
import re
import sys

root = Path(sys.argv[1]).expanduser() if len(sys.argv) > 1 else Path.home() / '.tyde' / 'tracing'
phases = set('ActorCreated ActorTerminated StopRequested StopDispatched StopSettled CancellationRequested CheckpointMissing CheckpointPublished ShutdownStarted AgentsCaptured AgentsPrepared AgentStopsSettled ArchiveFailed AccountingCommitted ShutdownSettled'.split())
classes = {'Running', 'Returned', 'Unwinding', 'Dropped'}
if not root.is_dir():
    raise SystemExit('Tyde tracing directory unavailable')
files = records = 0
try:
    paths = sorted(path for path in root.iterdir() if fnmatchcase(path.name, 'tyde-*.log*'))
except OSError:
    raise SystemExit('Unreadable Tyde tracing directory; export incomplete') from None
for path in paths:
    files += 1
    try:
        with path.open(encoding='utf-8') as stream:
            for line in stream:
                result = {}
                stamp = re.match(r'(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)\b', line)
                if stamp:
                    result['time'] = stamp[1]
                if ' server::restart_accounting:' in line:
                    phase = re.search(r'\bphase=(\w+)\b', line)
                    if not phase or phase[1] not in phases:
                        continue
                    result['phase'] = phase[1]
                    run = re.search(r'\bdiagnostic_run=([0-9a-f]{32})\b', line)
                    if not run:
                        raise SystemExit('Missing diagnostic process scope; export incomplete')
                    result['diagnostic_run'] = run[1]
                    state = re.search(r'\btermination=(\w+)\b', line)
                    if state and state[1] in classes:
                        result['termination'] = state[1]
                    for key in ['command_sent', 'acknowledged', 'actor_finished', 'checkpoint_present', 'checkpoint_channel_closed', 'success']:
                        value = re.search(r'\b' + key + r'=(true|false)\b', line)
                        if value:
                            result[key] = value[1] == 'true'
                    for key in ['accounting_actor', 'agents', 'missing_checkpoints']:
                        value = re.search(r'\b' + key + r'=(\d{1,20})\b', line)
                        if value:
                            result[key] = int(value[1])
                elif re.search(r' server::host:\s+(?:\d+:\s+)?host restart stopped without committing required accounting(?:\s|$)', line):
                    result['phase'] = 'LegacyAccountingRefused'
                elif re.search(r' server::host:\s+(?:\d+:\s+)?host restart shutdown completed; accounting committed and process scopes sealed(?:\s|$)', line):
                    result['phase'] = 'LegacyAccountingCommitted'
                elif re.search(r' (?:tauri_shell|tyde_shell_lib)::updates:\s+(?:\d+:\s+)?app update installation failed\b', line):
                    result['phase'] = 'LegacyUpdateFailed'
                    if 'The actor ended without a final task accounting checkpoint; durable observations were not replaced' in line:
                        result['failure'] = 'MissingCheckpoint'
                elif re.match(r"thread '[^'\n]*' (?:\(\d+\) )?panicked at ", line):
                    # An existing runtime hook header is not proof of which actor failed.
                    result = {'termination': 'RuntimePanicHeader', 'actor_attribution': 'unavailable'}
                else:
                    continue
                records += 1
                print(json.dumps(result, sort_keys=True))
    except (OSError, UnicodeError):
        raise SystemExit("Unreadable Tyde tracing input; export incomplete") from None
print(json.dumps({'files_read': files, 'records_exported': records, 'actor_classes_unavailable_in_older_logs': True}, sort_keys=True))
