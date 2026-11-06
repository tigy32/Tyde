import copy, hashlib, json, os, pathlib, re, stat, subprocess, sys, tempfile
B=pathlib.Path(__file__).resolve().parent
SCRIPT=B/'export-desktop-stalls.py'
record={'diagnostic': {'event':'event_loop_gap','interval_ms':2000,'expected_ms':1000,
    'focused':True,'longest_work':None,'pending_bridge_calls':0,
    'task_work':dict.fromkeys(('polls','poll_ms','longest_poll_ms','drops','drop_ms','longest_drop_ms'),0),
    'task_producers':[{'producer':'unattributed','work':dict.fromkeys(('polls','poll_ms','longest_poll_ms','drops','drop_ms','longest_drop_ms'),0)}],
    'activity':{'commands':[],'dispatch':None,'transitions':[]}},'dropped':0}
metrics=('polls','poll_ms','longest_poll_ms','drops','drop_ms','longest_drop_ms')
producers=('unattributed','chat_scroll_restore','chat_auto_scroll','chat_row_measure','tab_strip_visibility')
canary='private_content_/path_agent_id_argv_env_secret'
results=[]

def native_line(r,stamp='2026-11-02T18:10:00.000000Z'):
    return stamp+' INFO worker tauri_shell::ui_diagnostic: ui.diagnostic layer=frontend native_dropped=0 record='+json.dumps(r,separators=(',',':'))+'\n'

def run(name,records,script=SCRIPT,raw=None):
    with tempfile.TemporaryDirectory(prefix='tyde-export-producers-') as home:
        home=pathlib.Path(home);logs=home/'.tyde/tracing';logs.mkdir(parents=True);desktop=home/'Desktop';desktop.mkdir()
        body='2026-11-02T18:00:00Z INFO ui.diagnostic event=enabled version=0.9.6-beta.20\n'
        body+=''.join(native_line(r) for r in records) if raw is None else raw
        (logs/'tyde-shell.log.2026-11-02-18').write_text(body)
        env=os.environ.copy();env['HOME']=str(home)
        subprocess.run([sys.executable,str(script)],env=env,check=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        out=list(desktop.glob('Tyde-stalls-*.jsonl'));assert len(out)==1
        assert stat.S_IMODE(out[0].stat().st_mode)==0o600
        data=out[0].read_bytes();assert len(data)<=2097152 and canary.encode() not in data
        rows=[json.loads(line) for line in data.splitlines()];meta=rows[0]['export']
        assert meta['limits']==dict(files=48,head_bytes_per_file=65536,max_bytes_per_file=8388608,total_input_bytes=33554432,line_bytes=16384,output_bytes=2097152)
        assert meta['input_bytes']<=33554432 and meta['files_read']<=48
        gaps=[r for r in rows[1:] if r['kind']=='frontend_gap']
        results.append(dict(case=name,pass_=True,rows=len(rows)-1,bytes=len(data),invalid=meta.get('invalid_task_producers',0),invalid_sync_work=meta.get('invalid_sync_work',0)))
        return meta,gaps

meta,rows=run('synthetic_current_diagnostic_payload',[record])
assert rows[0]['record']['diagnostic']['task_producers']==record['diagnostic']['task_producers']
assert rows[0]['record']['diagnostic']['longest_work'] is None
assert any(r['producer']=='unattributed' for r in rows[0]['record']['diagnostic']['task_producers'])
legacy=copy.deepcopy(record);del legacy['diagnostic']['task_producers']
meta,rows=run('missing_old_field_remains_unknown',[legacy]);assert 'task_producers' not in rows[0]['record']['diagnostic'] and meta['invalid_task_producers']==0
full=copy.deepcopy(record);full['diagnostic']['task_producers']=[dict(producer=p,work={k:4294967295 for k in metrics}) for p in producers]
meta,rows=run('all_five_enums_u32_max',[full]);assert rows[0]['record']['diagnostic']['task_producers']==full['diagnostic']['task_producers']
empty=copy.deepcopy(record);empty['diagnostic']['task_producers']=[]
meta,rows=run('observed_empty_not_unknown',[empty]);assert rows[0]['record']['diagnostic']['task_producers']==[] and meta['invalid_task_producers']==0

bad=[]
bad.append(('unknown_producer',[dict(producer=canary,work={k:0 for k in metrics})]))
bad.append(('duplicate',[full['diagnostic']['task_producers'][0]]*2))
bad.append(('six_entries',full['diagnostic']['task_producers']+[full['diagnostic']['task_producers'][0]]))
bad.extend((name,value) for name,value in [('null',None),('object',{'producer':canary}),('string',canary),('bad_row',[None])])
for name,value in [('negative',-1),('above_u32',4294967296),('bool',True),('float',1.0),('string',canary),('null',None),('array',[canary]),('nan',float('nan'))]:
    rows=copy.deepcopy(full['diagnostic']['task_producers']);rows[0]['work']['poll_ms']=value;bad.append(('metric_'+name,rows))
rows=copy.deepcopy(full['diagnostic']['task_producers']);del rows[0]['work']['polls'];bad.append(('missing_metric',rows))
rows=copy.deepcopy(full['diagnostic']['task_producers']);rows[0]['work']['argv']=canary;bad.append(('extra_metric',rows))
rows=copy.deepcopy(full['diagnostic']['task_producers']);rows[0]['env']=canary;bad.append(('extra_row_field',rows))
for name,value in bad:
    r=copy.deepcopy(record);r['diagnostic']['task_producers']=value
    meta,rows=run(name,[r]);assert len(rows)==1 and 'task_producers' not in rows[0]['record']['diagnostic'] and meta['invalid_task_producers']==1 and meta['truncated_or_sampled']

privacy=copy.deepcopy(full);privacy['argv']=canary;privacy['diagnostic']['env']={canary:canary};privacy['diagnostic']['activity']={'commands':[{'frame':'send_message','age_ms':n} for n in range(9)],'dispatch':None,'transitions':[]}
meta,rows=run('unrelated_privacy_and_list_bounds',[privacy]);d=rows[0]['record']['diagnostic'];assert len(d['task_producers'])==5 and len(d['activity']['commands'])==4
# Retention and input/line budgets are exercised without host-speed verdicts.
raw=''.join(native_line(full,f'2026-11-02T18:{n//60:02d}:{n%60:02d}Z') for n in range(520))
meta,rows=run('unchanged_frontend_quota_and_newest_retention',[],raw=raw);assert len(rows)==512 and meta['matched_counts']['frontend_gap']==520 and meta['retained_counts']['frontend_gap']==512 and rows[0]['at']=='2026-11-02T18:00:08Z'
raw='2026-11-02T18:00:00Z '+canary*1000+'\n'+native_line(record)
meta,rows=run('oversized_line_explicit_skip',[],raw=raw);assert meta['skipped_lines']==1 and len(rows)==1
with tempfile.TemporaryDirectory(prefix='tyde-export-producers-large-') as h:
    home=pathlib.Path(h);logs=home/'.tyde/tracing';logs.mkdir(parents=True);(home/'Desktop').mkdir()
    for n in range(49):
        path=logs/f'tyde-shell.log.2026-11-{n//24+1:02d}-{n%24:02d}'
        with path.open('wb') as f:
            f.write(native_line(record).encode());f.seek(9*1024*1024);f.write(b'\n')
    env=os.environ.copy();env['HOME']=str(home)
    subprocess.run([sys.executable,str(SCRIPT)],env=env,check=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    files=list((home/'Desktop').glob('*.jsonl'));assert len(files)==1 and files[0].stat().st_size<=2097152
    m=json.loads(files[0].read_text().splitlines()[0])['export'];assert m['files_read']==48 and m['files_omitted']==1 and m['input_bytes']==33554432 and m['omitted_file_bytes']>0 and m['truncated_or_sampled']
    results.append(dict(case='unchanged_48_file_8MiB_file_32MiB_total_input_bounds',pass_=True))
# V3 extends the same process/file-boundary runner; no application or provider run.
sync_record=copy.deepcopy(record)
sync_record['diagnostic']['sync_work']=[dict(operation='history_project_name',work=dict.fromkeys(('calls','items','max_items','work_ms','longest_ms'),1))]
source=(B.parent/'host-config/src/diagnostics.rs').read_text().split('pub enum UiOperation {',1)[1].split('}',1)[0]
source_operations={re.sub(r'(?<!^)(?=[A-Z])','_',v).lower() for v in re.findall(r'^    (\w+),$',source,re.M)}
exporter=SCRIPT.read_text()
# Parse only the literal table, never execute the exporter inside this process.
import ast
admitted=ast.literal_eval(exporter.split('sync_operations = frozenset(',1)[1].split(')\n',1)[0])
assert set(admitted)==source_operations-{'bridge','task_poll','task_drop'}, 'Export admission drifted from UiOperation'
sync_metrics=('calls','items','max_items','work_ms','longest_ms')
sync_operations=('host_receive','host_dispatch','composer_input','composer_resize','global_keydown','markdown','syntax_highlight','worker_submit','worker_receive','file_tree_build','chat_row_projection','chat_context_scan','chat_context_reported_scan','chat_ink_layout','chat_virtual_window','chat_tab_binding','agents_sidebar_projection','history_project_name','history_page_projection','history_card_build','project_switch','tab_open','tab_close','tab_reveal')
meta,rows=run('synthetic_current_sync_totals',[sync_record])
assert rows[0]['record']['diagnostic']['sync_work']==sync_record['diagnostic']['sync_work'] and meta['invalid_sync_work']==0
legacy=copy.deepcopy(record);legacy['diagnostic'].pop('sync_work',None)
meta,rows=run('sync_absent_is_unknown_not_empty',[legacy]);assert 'sync_work' not in rows[0]['record']['diagnostic'] and meta['invalid_sync_work']==0
valid=copy.deepcopy(full)
valid['diagnostic']['sync_work']=[dict(operation=p,work={k:4294967295 for k in sync_metrics}) for p in sync_operations]
valid['diagnostic']['longest_work']=dict(operation='history_project_name',duration_ms=100,frame=None,bytes=0,items=1)
meta,rows=run('all_24_sync_operations_and_five_producers_u32_max',[valid]);d=rows[0]['record']['diagnostic']
assert d['sync_work']==valid['diagnostic']['sync_work'] and len(d['task_producers'])==5 and d['longest_work']['operation']=='history_project_name'
for operation in sync_operations:
    candidate=copy.deepcopy(valid)
    candidate['diagnostic']['longest_work']['operation']=operation
    meta,rows=run('longest_'+operation,[candidate])
    assert rows[0]['record']['diagnostic']['longest_work']['operation']==operation
empty=copy.deepcopy(valid);empty['diagnostic']['sync_work']=[]
meta,rows=run('sync_observed_empty_preserved',[empty]);assert rows[0]['record']['diagnostic']['sync_work']==[] and meta['invalid_sync_work']==0
bad_sync=[('unknown_operation',[dict(operation=canary,work={k:0 for k in sync_metrics})]),('duplicate_operation',[valid['diagnostic']['sync_work'][0]]*2),('33_entries',[valid['diagnostic']['sync_work'][0]]*33)]
for operation in ('bridge','task_poll','task_drop'):
    bad_sync.append(('non_sync_'+operation,[dict(operation=operation,work={k:0 for k in sync_metrics})]))
for name,value in [('null',None),('object',{'operation':canary}),('string',canary),('bool',True),('bad_row',[None]),('array_row',[[canary]])]:bad_sync.append((name,value))
for name,value in [('negative',-1),('above_u32',4294967296),('bool',True),('float',1.0),('string',canary),('null',None),('array',[canary]),('nan',float('nan')),('infinity',float('inf'))]:
    candidate=copy.deepcopy(valid['diagnostic']['sync_work']);candidate[-1]['work']['work_ms']=value;bad_sync.append(('metric_'+name,candidate))
for metric in sync_metrics:
    candidate=copy.deepcopy(valid['diagnostic']['sync_work']);candidate[-1]['work'][metric]=False;bad_sync.append(('bool_'+metric,candidate))
candidate=copy.deepcopy(valid['diagnostic']['sync_work']);del candidate[-1]['work']['calls'];bad_sync.append(('missing_metric',candidate))
candidate=copy.deepcopy(valid['diagnostic']['sync_work']);candidate[-1]['work']['argv']=canary;bad_sync.append(('extra_metric',candidate))
candidate=copy.deepcopy(valid['diagnostic']['sync_work']);candidate[-1]['work']=canary;bad_sync.append(('invalid_work_shape',candidate))
candidate=copy.deepcopy(valid['diagnostic']['sync_work']);candidate[-1]['env']=canary;bad_sync.append(('extra_row_field',candidate))
candidate=copy.deepcopy(valid['diagnostic']['sync_work']);del candidate[-1]['operation'];bad_sync.append(('missing_operation',candidate))
candidate=copy.deepcopy(valid['diagnostic']['sync_work']);candidate[-1]['operation']=[canary];bad_sync.append(('invalid_operation_type',candidate))
for name,value in bad_sync:
    candidate=copy.deepcopy(valid);candidate['diagnostic']['sync_work']=value
    meta,rows=run('sync_'+name,[candidate]);d=rows[0]['record']['diagnostic']
    assert len(rows)==1 and 'sync_work' not in d and meta['invalid_sync_work']==1 and meta['truncated_or_sampled']
    assert d['task_producers']==valid['diagnostic']['task_producers'] and meta['invalid_task_producers']==0
privacy=copy.deepcopy(valid);privacy['argv']=canary;privacy['diagnostic']['native_id']=canary
privacy['diagnostic']['activity']={'commands':[{'frame':'send_message','age_ms':n} for n in range(9)],'dispatch':{'frame':canary,'age_ms':1},'transitions':[]}
meta,rows=run('sync_privacy_and_independent_list_limits',[privacy]);d=rows[0]['record']['diagnostic']
assert len(d['sync_work'])==24 and len(d['task_producers'])==5 and len(d['activity']['commands'])==4 and d['activity']['dispatch']['frame']=='<omitted>'
startup={'diagnostic':{'event':'startup','phase':'wasm_entered','since_wasm_ms':0,'sync_work':[{canary:canary}]},'dropped':0}
meta,rows=run('startup_cannot_smuggle_sync_block',[startup]);assert not rows and meta['invalid_sync_work']==0
# Large valid records exercise the existing total-output budget without enlarging it.
large=copy.deepcopy(privacy)
large['diagnostic']['activity']={'commands':[{'frame':'send_message','age_ms':4294967295}]*4,'dispatch':{'frame':'project_notify','age_ms':4294967295},'transitions':[{'transition':'project_switch','age_ms':4294967295}]*4}
raw=''.join(native_line(large,f'2026-11-02T19:{n//60:02d}:{n%60:02d}Z') for n in range(520))
meta,rows=run('sync_large_record_newest_and_total_output_budget',[],raw=raw)
assert meta['matched_counts']['frontend_gap']==520 and meta['retained_counts']['frontend_gap']==512 and meta['output_rows_omitted']>0 and meta['truncated_or_sampled']
assert rows[-1]['at']=='2026-11-02T19:08:39Z'
print('EXPORT_BOUNDARY_PASS',len(results),'cases',hashlib.sha256(SCRIPT.read_bytes()).hexdigest())
