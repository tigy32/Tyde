#!/usr/bin/env python3
"""Render the Tyde Book using only the Python standard library."""
import html
import json
import re
import sys
from html.parser import HTMLParser
from urllib.parse import unquote, urlsplit
from pathlib import Path

ROOT = Path(__file__).resolve().parent
if sys.argv[1:] not in ([], ['--check']):
    raise SystemExit('Usage: python3 docs/build.py [--check]')
check = sys.argv[1:] == ['--check']
outputs = {}
pages = json.loads((ROOT / 'contents.json').read_text())
search = []
repository_book = ['# Tyde User Guide\n\nInstructions for using Tyde after installation.\n']
for index, page in enumerate(pages):
    slug, title = page['slug'], page['title']
    body = (ROOT / 'chapters' / f'{slug}.html').read_text()
    sections = re.findall(r'<h2 id="([^"]+)">(.*?)</h2>', body)
    navigation = ''
    group = None
    for number, item in enumerate(pages):
        if item['part'] != group:
            group = item['part']
            navigation += f'<p class="nav-group">{html.escape(group)}</p>'
        current = ' aria-current="page"' if item == page else ''
        navigation += f'<a href="{item["slug"]}.html"{current}><span>{number + 1:02}</span>{html.escape(item["title"])}</a>'
    adjacent = ''
    for offset, label in [(-1, 'Previous chapter'), (1, 'Next chapter')]:
        position = index + offset
        if 0 <= position < len(pages):
            item = pages[position]
            adjacent += f'<a href="{item["slug"]}.html"><small>{label}</small>{html.escape(item["title"])} <span aria-hidden="true">↗</span></a>'
        else:
            adjacent += '<span></span>'
    toc = ''.join(f'<a href="#{anchor}">{heading}</a>' for anchor, heading in sections)
    document = f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} — Tyde User Guide</title><meta name="description" content="{html.escape(page['description'], quote=True)}">
<meta name="color-scheme" content="light dark"><link rel="icon" href="assets/icon.png"><link rel="stylesheet" href="assets/book.css">
<script src="assets/search-index.js" defer></script><script src="assets/book.js" defer></script></head>
<body><a class="skip" href="#main">Skip to content</a>
<header class="topbar"><a class="brand" href="index.html"><img src="assets/icon.png" alt="" width="28" height="28">tyde<span class="brand-divider">/</span><span class="book-label">User Guide</span></a>
<div class="top-actions"><button class="search-open" hidden>Search the guide <kbd>/</kbd></button><button class="theme-toggle" hidden aria-label="Switch color theme">◐</button><a class="download" href="https://tycode.dev/tyde.html">Downloads <span aria-hidden="true">↗</span></a></div></header>
<div class="layout"><details class="chapter-menu" open><summary>Chapters</summary><nav aria-label="Chapters">{navigation}</nav><p class="nav-foot">TYDE USER GUIDE<br><span>Features and instructions.</span></p></details>
<main id="main"><div class="eyebrow">{html.escape(page['part'])} <span>/ {index + 1:02}</span></div><h1>{html.escape(title)}</h1><p class="lede">{html.escape(page['description'])}</p>{body}
<nav class="pagination" aria-label="Chapter pagination">{adjacent}</nav><footer>Tyde User Guide <a href="https://github.com/tigy32/Tyde/issues">Report a documentation issue ↗</a></footer></main>
<aside class="on-page"><p>IN THIS CHAPTER</p>{toc}<div class="aside-note">Use the chapter list to find a feature.<br>Search for a control or task.</div></aside></div>
<dialog id="search-dialog" aria-labelledby="search-title"><div class="search-heading"><h2 id="search-title">Search the guide</h2><button class="search-close" aria-label="Close search">Close</button></div><label class="sr-only" for="search-input">Search chapters and content</label><input id="search-input" type="search" placeholder="Try remote hosts, skills, or review…" autocomplete="off"><p id="search-status" role="status"></p><div id="search-results"></div></dialog>
</body></html>'''
    outputs[ROOT / f'{slug}.html'] = document
    repository_body = re.sub(r'id="([^"]+)"', lambda match: f'id="{slug}-{match[1]}"', body)
    repository_body = re.sub(r'href="#([^"]+)"', lambda match: f'href="#{slug}-{match[1]}"', repository_body)
    repository_body = re.sub(r'href="([a-z-]+)\.html(?:#([^"]+))?"', lambda match: f'href="#{match[1]}' + (f'-{match[2]}' if match[2] else '') + '"', repository_body)
    repository_book.append(f'<h2 id="{slug}">{html.escape(title)}</h2>\n\n' + repository_body)
    plain = html.unescape(re.sub(r'<[^>]+>', ' ', body))
    search.append({'title': title, 'url': f'{slug}.html', 'text': re.sub(r'\s+', ' ', page['description'] + ' ' + plain)})
outputs[ROOT / 'assets' / 'search-index.js'] = 'window.TYDE_BOOK_SEARCH = ' + json.dumps(search, ensure_ascii=False) + ';\n'
outputs[ROOT / 'book.md'] = '\n\n'.join(repository_book)
class Links(HTMLParser):
    def __init__(self):
        super().__init__()
        self.ids = set()
        self.references = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if 'id' in attrs:
            if attrs['id'] in self.ids:
                raise ValueError(f"Duplicate anchor: {attrs['id']}")
            self.ids.add(attrs['id'])
        for name in ('href', 'src'):
            if attrs.get(name):
                self.references.append(attrs[name])

parsed = {}
for path, content in outputs.items():
    if path.suffix == '.html':
        parser = Links()
        parser.feed(content)
        parsed[path] = parser
for path, parser in parsed.items():
    for reference in parser.references:
        url = urlsplit(reference)
        if url.scheme or url.netloc:
            continue
        target = (path.parent / unquote(url.path)).resolve() if url.path else path
        if target not in outputs and not target.is_file():
            raise SystemExit(f'Missing local target in {path.name}: {reference}')
        if url.fragment and target in parsed and unquote(url.fragment) not in parsed[target].ids:
            raise SystemExit(f'Missing anchor in {path.name}: {reference}')
stale = []
for path, content in outputs.items():
    if check:
        if not path.exists() or path.read_text() != content:
            stale.append(str(path.relative_to(ROOT)))
    else:
        path.write_text(content)
if stale:
    raise SystemExit('Re-render with python3 docs/build.py: ' + ', '.join(stale))
print(f"{'Verified' if check else 'Rendered'} {len(pages)} chapters and local links")
