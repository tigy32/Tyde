#!/usr/bin/env python3
"""Opt-in, read-only GitHub download regression; never part of dev.sh check.

Uses existing gh auth and production download/receipt/signature guards. It does
not run recovery context, package, Apple, upload, deploy, or publish operations.
An unlanded controller cannot pass recovery's main-ancestry guard; this probe
establishes only the real HTTP download boundary and selected retained bytes.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
from pathlib import Path
import sys
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--controller-file', type=Path, default=Path(__file__).with_name('macos_release.py'))
    for name in ('repository', 'tag', 'source-sha', 'producer-sha', 'run-id', 'artifact-id',
                 'fragments-run-id', 'target', 'receipt-sha256'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location('download_controller', args.controller_file)
    controller = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(controller)
    args.git_events = []
    args.retained = args.output / 'retained'
    try:
        controller.selection(args)
        if args.output.exists():
            raise controller.ControllerError('Probe requires a new output directory')
        args.output.mkdir(parents=True, mode=0o700)
        ref = controller.api(args, f'git/ref/tags/{args.tag}')['object']
        if ref['type'] == 'tag':
            ref = controller.api(args, f"git/tags/{ref['sha']}")['object']
        if ref['type'] != 'commit' or ref['sha'] != args.source_sha:
            raise controller.ControllerError('Live source tag mismatch')
        metadata = controller.download(args, args.artifact_id, args.run_id, args.retained)
        data = controller.receipt(args)
        if (controller.digest(args.retained / 'receipt.json') != args.receipt_sha256
                or data['controller_sha'] != args.producer_sha
                or data['checkpoint_controller_sha'] != args.producer_sha
                or metadata['workflow_run']['head_sha'] != args.producer_sha
                or str(data['checkpoint_run_id']) != args.run_id
                or str(data['run_id']) != args.run_id):
            raise controller.ControllerError('Selected original checkpoint receipt/provenance mismatch')
        controller.provenance(args, args.run_id, args.producer_sha)
        controller.provenance(args, args.fragments_run_id)
        artifacts = controller.api(args, f'actions/runs/{args.fragments_run_id}/artifacts?per_page=100')
        if artifacts['total_count'] > 100:
            raise controller.ControllerError('Too many fragment artifacts')
        release = controller.draft(args)
        # Read the updater key from the exact frozen commit, never current source.
        config = args.output / 'frozen-updater-config.json'
        config.write_bytes(controller.command(['git', '-C', controller.CONTROLLER, 'show',
                          f'{args.source_sha}:frontend/tauri-shell/tauri.conf.json']))
        fragments = 0
        selected = None
        for target in controller.update_manifest.TARGETS:
            if target == args.target:
                continue
            matches = [a for a in artifacts['artifacts'] if a['name'] == f'update-{target}']
            if len(matches) != 1:
                raise controller.ControllerError('Missing or ambiguous same-source fragment')
            folder = args.output / target
            controller.download(args, matches[0]['id'], args.fragments_run_id, folder)
            fragment = json.loads((folder / f'update-{target}.json').read_text())
            fragments += 1
            if target == 'x86_64-apple-darwin':
                selected = next(iter(fragment['platforms'].values()))
        if selected is None:
            raise controller.ControllerError('Missing Intel updater fragment')
        prefix = f'https://github.com/{args.repository}/releases/download/{args.tag}/'
        matches = [a for a in release['assets'] if prefix + a['name'] == selected['url']]
        if len(matches) != 1:
            raise controller.ControllerError('Missing or ambiguous selected draft updater')
        asset = matches[0]
        payload = args.output / asset['name']
        payload.write_bytes(controller.download_asset(args, asset))
        controller.verify_update(config, payload, selected['signature'])
        result = {'source_sha': args.source_sha, 'producer_sha': args.producer_sha,
                  'transport_digest': metadata['digest'], 'transport_bytes': metadata['size_in_bytes'],
                  'receipt_sha256': controller.digest(args.retained / 'receipt.json'),
                  'payload_sha256': data['payloads'], 'phase': data['phase'],
                  'apple_statuses': {k: v.get('status') for k, v in data['submissions'].items()},
                  'same_source_fragments_verified': fragments, 'signed_draft_updater_sha256': controller.digest(payload),
                  'scope': 'read-only downloads; no recovery context or Apple execution'}
        controller.save(args.output / 'result.json', result)
        print(json.dumps(result, indent=2))
        return 0
    except (controller.ControllerError, OSError, ValueError, KeyError, RuntimeError, zipfile.BadZipFile) as error:
        message = str(error) if isinstance(error, controller.ControllerError) else type(error).__name__
        print('Read-only GitHub download probe: FAIL: ' + message, file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
