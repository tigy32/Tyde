#!/usr/bin/env python3
"""Build-bound native bootstrap policy; production has one canonical origin."""
import argparse
import json
from urllib.parse import urlsplit

parser = argparse.ArgumentParser()
parser.add_argument('--qa-origin')
args = parser.parse_args()
origin = 'https://tycode.dev'
if args.qa_origin is not None:
    raw = args.qa_origin
    if any(ord(c) <= 32 or ord(c) >= 127 for c in raw):
        parser.error('QA origin must be an ASCII HTTPS origin')
    try:
        u = urlsplit(raw)
        port = u.port
    except ValueError:
        parser.error('QA origin is invalid')
    if u.scheme != 'https' or not u.hostname or u.username or u.password or u.path not in ('', '/') or u.query or u.fragment:
        parser.error('QA configuration requires one HTTPS origin, without a path or credentials')
    host = u.hostname.lower()
    authority = f'[{host}]' if ':' in host else host
    origin = 'https://' + authority + (f':{port}' if port not in (None, 443) else '')
    if host == 'tycode.dev':
        parser.error('QA requires a separate origin; production remains canonical')
qa = args.qa_origin is not None
print(json.dumps({'kind': 'qa' if qa else 'production', 'origin': origin,
                  'bootstrap_url': origin + '/tyde/?native=1',
                  'publisher_url': origin + '/tyde/manifest.json',
                  'domain': urlsplit(origin).hostname,
                  'debug_bundle_identifier': 'dev.tyde.mobile.thinbootstrapqa' if qa else 'dev.tyde.mobile.nativeqa',
                  'display_name': 'Tyde QA' if qa else 'Tyde'}, indent=2))
