import collections, datetime, heapq, json, os, pathlib, re, stat, tempfile
root = pathlib.Path.home() / '.tyde' / 'tracing'
desktop = pathlib.Path.home() / 'Desktop'
if not root.is_dir() or root.is_symlink() or not desktop.is_dir():
    raise SystemExit('Expected ~/.tyde/tracing and ~/Desktop; no alternate path searched. A configured tracing override must be supplied explicitly, not guessed.')
keys = set('diagnostic event phase since_wasm_ms interval_ms expected_ms focused longest_work operation duration_ms frame bytes items pending_bridge_calls task_work polls poll_ms longest_poll_ms drops drop_ms longest_drop_ms activity commands dispatch transitions transition age_ms dropped'.split())
words = set('startup event_loop_gap wasm_entered mount_begin mount_returned highlight_warmup_begin highlight_warmup_end host_listeners_ready configured_hosts_refresh_returned host_connections_scheduled ready_acknowledged host_receive host_dispatch bridge composer_input composer_resize global_keydown markdown syntax_highlight worker_submit worker_receive file_tree_build chat_row_projection chat_context_scan chat_context_reported_scan chat_ink_layout chat_virtual_window chat_tab_binding agents_sidebar_projection task_poll task_drop project_switch tab_open tab_close tab_reveal project_create project_rename project_reorder project_add_root project_delete_root project_delete workbench_create workbench_remove workbench_recover project_operation project_bootstrap project_notify project_event project_file_contents host_bootstrap spawn_agent load_agent fetch_session_history list_sessions delete_session close_agent agent_move agent_project_changed send_message interrupt command_error reject welcome'.split())
words.update(('history_project_name', 'history_page_projection', 'history_card_build'))
phases = set('ProcessEnvironment WebviewDefaults BuildShell Setup HostStore SpawnHost NativeMedia Updates'.split())
life = set('page_load_started page_load_finished frontend_ready visible hidden focus blur page_show page_hide window_focus'.split())
meta = {'schema': 1, 'existing_logs_only': True, 'path': '~/.tyde/tracing/tyde-shell.log.YYYY-MM-DD-HH', 'limits': {'files':48,'head_bytes_per_file':65536,'max_bytes_per_file':8388608,'total_input_bytes':33554432,'line_bytes':16384,'output_bytes':2097152}, 'files_read':0,'input_bytes':0,'omitted_file_bytes':0,'skipped_lines':0,'unlisted_values_omitted':0,'invalid_task_producers':0,'invalid_sync_work':0,'versions':[]}
quotas = dict([('enabled',48),('frontend_gap',512),('frontend_startup',96),('native',128),('startup',96),('lifecycle',64),('focus',16),('host_io',64)])
buckets = {k:[] for k in quotas}
seen = collections.Counter(); sequence = 0

def clean(value, depth=0):
    if depth>7: return None
    if value is None or type(value) is bool: return value
    if type(value) is int: return value if 0<=value<=4294967295 else None
    if isinstance(value,str):
        if value in words: return value
        meta['unlisted_values_omitted']+=1; return '<omitted>'
    if isinstance(value,list): return [clean(v,depth+1) for v in value[:4]]
    if isinstance(value,dict): return {k:clean(v,depth+1) for k,v in value.items() if k in keys}
    return None

task_producers = frozenset(('unattributed','chat_scroll_restore','chat_auto_scroll','chat_row_measure','tab_strip_visibility'))
task_metrics = ('polls','poll_ms','longest_poll_ms','drops','drop_ms','longest_drop_ms')

def clean_task_producers(value):
    if not isinstance(value,list) or len(value)>5:
        meta['invalid_task_producers']+=1
        return None
    rows=[]; origins=set()
    for row in value:
        if not isinstance(row,dict) or set(row)!= {'producer','work'}:
            meta['invalid_task_producers']+=1
            return None
        producer=row['producer'];work=row['work']
        if not isinstance(producer,str) or producer not in task_producers or producer in origins:
            meta['invalid_task_producers']+=1
            return None
        if not isinstance(work,dict) or set(work)!=set(task_metrics) or any(type(work[key]) is not int or not 0<=work[key]<=4294967295 for key in task_metrics):
            meta['invalid_task_producers']+=1
            return None
        origins.add(producer)
        rows.append({'producer':producer,'work':{key:work[key] for key in task_metrics}})
    return rows

sync_operations = frozenset(('host_receive','host_dispatch','composer_input','composer_resize','global_keydown','markdown','syntax_highlight','worker_submit','worker_receive','file_tree_build','chat_row_projection','chat_context_scan','chat_context_reported_scan','chat_ink_layout','chat_virtual_window','chat_tab_binding','agents_sidebar_projection','history_project_name','history_page_projection','history_card_build','project_switch','tab_open','tab_close','tab_reveal'))
sync_metrics = ('calls','items','max_items','work_ms','longest_ms')

def clean_sync_work(value):
    if not isinstance(value,list) or len(value)>32:
        meta['invalid_sync_work']+=1
        return None
    rows=[]; operations=set()
    for row in value:
        if not isinstance(row,dict) or set(row)!= {'operation','work'}:
            meta['invalid_sync_work']+=1
            return None
        operation=row['operation'];work=row['work']
        if not isinstance(operation,str) or operation not in sync_operations or operation in operations:
            meta['invalid_sync_work']+=1
            return None
        if not isinstance(work,dict) or set(work)!=set(sync_metrics) or any(type(work[key]) is not int or not 0<=work[key]<=4294967295 for key in sync_metrics):
            meta['invalid_sync_work']+=1
            return None
        operations.add(operation)
        rows.append({'operation':operation,'work':{key:work[key] for key in sync_metrics}})
    return rows

def field(line,key,pattern):
    m=re.search(r'\b'+key+r'=(' + pattern + r')(?=\s|$)',line)
    return m.group(1) if m else None

def process(raw):
    global sequence
    if len(raw)>16384: meta['skipped_lines']+=1; return
    line=raw.decode('utf-8','replace');stamp=re.match(r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z',line)
    if not stamp: return
    row={'at':stamp.group()}; category=None
    if 'ui.diagnostic layer=frontend ' in line and ' record=' in line:
        try: data=json.loads(line.split(' record=',1)[1])
        except (ValueError,RecursionError): meta['skipped_lines']+=1; return
        if not isinstance(data,dict) or not isinstance(data.get('diagnostic'),dict): return
        event=data['diagnostic'].get('event')
        if event not in ('startup','event_loop_gap'): return
        category='frontend_gap' if event=='event_loop_gap' else 'frontend_startup';row['record']=clean(data)
        if event=='event_loop_gap' and 'task_producers' in data['diagnostic']:
            producers=clean_task_producers(data['diagnostic']['task_producers'])
            if producers is not None:row['record']['diagnostic']['task_producers']=producers
        if event=='event_loop_gap' and 'sync_work' in data['diagnostic']:
            work=clean_sync_work(data['diagnostic']['sync_work'])
            if work is not None:row['record']['diagnostic']['sync_work']=work
    elif 'ui.diagnostic event=enabled ' in line:
        version=field(line,'version',r'\d{1,3}\.\d{1,3}\.\d{1,3}(?:-beta\.\d{1,5})?')
        if not version:return
        category='enabled';row.update(event='enabled',version=version)
        if version not in meta['versions']:meta['versions'].append(version)
    elif 'ui.diagnostic layer=native ' in line:
        event=field(line,'event',r'dispatch_pending|dispatch_completed|dispatch_failed|dispatch_cancelled|web_process_terminated')
        if not event:return
        category='native';row['event']=event
    elif 'ui.startup layer=native ' in line:
        phase=field(line,'phase',r'[A-Za-z]+');event=field(line,'event',r'begin|end')
        if phase not in phases or not event:return
        category='startup';row.update(phase=phase,event=event)
    elif 'webview.lifecycle event=' in line:
        event=field(line,'event',r'[a-z_]+')
        if event not in life:return
        category='focus' if event in ('focus','blur','window_focus') else 'lifecycle';row['event']=event
    elif 'host.config event=' in line or 'host.connect event=' in line:
        event=field(line,'event',r'ui_thread|invoke_dispatch|disk_read|disk_write|task_start|ssh_capture_start|log_loss')
        if not event:return
        category='host_io';row['event']=event
        thread=re.search(r'\bThreadId\((\d{1,9})\)',line)
        if thread:row['thread']=int(thread.group(1))
    if category is None:return
    for key in ('native_dropped','dropped','elapsed_ms','queue_ms','duration_ms','since_start_ms','generation','file_dropped_lines','native_probe_ms','frontend_probe_ms','frontend_gap_ms','records_per_minute','retention_files'):
        v=field(line,key,r'\d{1,12}')
        if v is not None:row[key]=int(v)
    for key,pattern in [('focused','true|false'),('outcome','complete|aborted')]:
        v=field(line,key,pattern)
        if v is not None:row[key]=v
    sequence+=1;row['kind']=category;row['_order']=(-source_rank,sequence);seen[category]+=1
    heapq.heappush(buckets[category],(row['at'],row['_order'],row))
    if len(buckets[category])>quotas[category]:heapq.heappop(buckets[category])

files=sorted(p for p in root.glob('tyde-shell.log.*') if re.fullmatch(r'tyde-shell\.log\.\d{4}-\d\d-\d\d-\d\d',p.name))
meta['files_omitted']=max(0,len(files)-48);files=files[-48:]
remaining=33554432
for source_rank,path in enumerate(reversed(files)):
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(fd,'rb') as f:
        info=os.fstat(f.fileno())
        if not stat.S_ISREG(info.st_mode):meta['skipped_lines']+=1;continue
        size=info.st_size;meta['files_read']+=1
        allow=min(size,8388608,remaining);remaining-=allow
        if allow==size:ranges=[(0,size)]
        elif allow>65536:ranges=[(0,65536),(size-(allow-65536),allow-65536)]
        else:ranges=[(size-allow,allow)] if allow else []
        meta['omitted_file_bytes']+=size-sum(n for _,n in ranges)
        for offset,n in ranges:
            f.seek(offset);data=f.read(n);meta['input_bytes']+=len(data)
            lines=data.splitlines()
            if offset and lines:lines=lines[1:]
            if offset+n<size and lines:lines=lines[:-1]
            for raw in lines:process(raw)
if not meta['files_read']:raise SystemExit('No existing documented shell logs found; nothing exported.')
rows=sorted((entry[2] for bucket in buckets.values() for entry in bucket),key=lambda r:(r['at'],r['_order']))
meta['matched_counts']=dict(seen);meta['retained_counts']={k:len(v) for k,v in buckets.items()};meta['output_rows_omitted']=0
encoded=[];budget=1900000
for row in reversed(rows):
    row.pop('_order');line=json.dumps(row,separators=(',',':'))+'\n';n=len(line.encode())
    if n>budget:meta['output_rows_omitted']+=1;continue
    encoded.append(line);budget-=n
meta['version_missing']=not bool(meta['versions']);meta['truncated_or_sampled']=bool(meta['files_omitted'] or meta['omitted_file_bytes'] or meta['skipped_lines'] or meta['invalid_task_producers'] or meta['invalid_sync_work'] or meta['output_rows_omitted'] or any(seen[k]>len(v) for k,v in buckets.items()))
payload=json.dumps({'export':meta},separators=(',',':'))+'\n'+''.join(reversed(encoded))
if len(payload.encode())>2097152:raise SystemExit('Sanitized output exceeded its bound; no export written.')
fd,name=tempfile.mkstemp(prefix='Tyde-stalls-'+datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'-',suffix='.jsonl',dir=desktop)
with os.fdopen(fd,'w',encoding='utf-8') as out:out.write(payload)
print('Sanitized existing-log export: '+name)
print('No reproduction, app control, raw logs, environment, process arguments, identifiers or error payloads exported.')
