"""Regenerate synthetic exploration workloads from measured trace marginals."""
import json,pathlib,math
out=pathlib.Path(__file__).resolve().parent
stats=json.loads((out/'measurements/distributions.json').read_text())
matrix=[]
def q(ds,k,p):return stats[ds]['distributions'][k][f'p{p}']
def fill(n):return ('Synthetic payload for replay measurements. '*(n//42+2))[:n]
def group(n,replies,user='Continue synthetic task {turn}.',setup=False):
    d=dict(repeat=n,user=user,replies=replies)
    if setup:d['setup']=True
    return d
def write(shape,tier,protocol,turns,note,**params):
    name=f'{shape}-{tier}'
    d=dict(format='lash-perf-replay/1',name=name,description='Explore only. provenance: '+note,protocol=protocol,turns=turns)
    (out/(name+'.json')).write_text(json.dumps(d,indent=2)+'\n')
    matrix.append(dict(name=name,protocol=protocol,turns=sum(g['repeat'] for g in turns),steady_turns=sum(g['repeat'] for g in turns if not g.get('setup')),**params))
def tool(key,n,inp=0):return dict(tool='tools.fixture_large',args=dict(key=key+fill(inp),bytes=n))
def seq(calls,n,inp,key='read-{turn}'):
    return [dict(tool_calls=[tool(key+f'-{i}',n,inp)]) for i in range(calls)]
for tier,p in [('small',10),('typical',50),('heavy',99)]:
    n=q('chat','user_turns',p);answer=q('chat','model_output_bytes',p)
    write('chat',tier,'standard',[group(n,[dict(text=fill(answer)+' {turn}')],fill(q('chat','user_input_bytes',p)))],f'WildChat-1M {stats["chat"]["revision"]} p{p} user turns, user bytes and assistant bytes; independent marginals.',output_bytes=answer)
    steps=q('swe','model_calls_session',p);calls=2 if tier=='small' else 4;n=math.ceil(steps/calls);b=q('swe','tool_output_bytes',p);inp=q('swe','tool_input_bytes',p)
    replies=seq(calls,b,inp)+[dict(text=fill(q('swe','model_output_bytes',p))+' {turn}')]
    full,tail=divmod(steps,calls)
    groups=[group(1,replies,fill(q('swe','user_input_bytes',p)))]
    if full>1:groups.append(group(full-1,replies))
    if tail:groups.append(group(1,seq(tail,b,inp)+[dict(text=fill(q('swe','model_output_bytes',p))+' {turn}')]))
    write('coding',tier,'standard',groups,f'SWE-agent-trajectories {stats["swe"]["revision"]} p{p} action count and tool/model bytes. Pack {calls} sequential actions per Lash turn; final partial turn preserves the exact action count. Final text adds one model call per turn; padding approximates command bytes before fixture metadata.',calls_per_turn=calls,total_tools=steps,output_bytes=b)
    n=q('jupyter','model_calls_session',p);preview=q('jupyter','tool_output_bytes',p);length=q('jupyter','code_cell_bytes',p)
    state_p=90 if tier=='heavy' else p
    state=8*q('jupyter','printed_dataframe_cells_session',state_p)
    b=math.ceil(state/n)
    for lang,protocol in [('ts','typescript'),('py','python')]:
        if lang=='ts':
            setup=['let chunks = [];','let retained = 0;','await control.finish({ ready: true });']
            source=[f'const chunk = await tools.fixtureLarge({{ key: "data-{{turn}}", bytes: {b} }});','chunks.push(chunk.text);','retained = retained + chunk.text.length;','await control.finish({ retained: retained, chunks: chunks.length, preview: chunk.text.slice(0, '+str(preview)+') });']
            comment='// '
        else:
            setup=['chunks = []','retained = 0','await control.finish({"ready": True})']
            source=[f'chunk = await tools.fixture_large({{"key": "data-{{turn}}", "bytes": {b}}})','chunks.append(chunk["text"])','retained = retained + len(chunk["text"])','await control.finish({"retained": retained, "chunks": len(chunks), "preview": chunk["text"][:'+str(preview)+']})']
            comment='# '
        # Pad source with synthetic comments to the measured cell length where it fits.
        missing=length-len('\n'.join(source))-1
        if missing>=len(comment):source.insert(0,comment+fill(missing-len(comment)))
        write('analysis-'+lang,tier,protocol,[group(1,[dict(cell=setup)],setup=True),group(n,[dict(cell=source)])],f'Jupyter agent {stats["jupyter"]["revision"]} non_thinking p{p} model calls mapped to turns, visible output bytes as previews, and code bytes (minimum executable source may exceed small cells). Growing synthetic table = 8 bytes per printed dataframe cell at p{state_p}, divided across turns. Native dataframe heap size is unobserved; backing chunks model file data, not logged tool output.',calls_per_turn=1,output_bytes=b,retained_bytes=n*b,preview_bytes=preview,state_percentile=state_p,cell_bytes=len('\n'.join(source)))
    n=q('tau','user_turns',p);calls={'small':1,'typical':2,'heavy':6}[tier]
    source=[]
    for i in range(calls):source.append(f'const phase{i} = await tools.fixtureSlow({{ key: "phase-{{turn}}-{i}", ms: 5 }});')
    source+=['progress.push({turn});','await control.finish({ completed: progress.length });']
    write('workflow',tier,'typescript',[group(1,[dict(cell=['let progress = [];','await control.finish({ ready: true });'])],setup=True),group(n,[dict(cell=source)])],f'tau-bench {stats["tau"]["revision"]} p{p} user turns. Sequential cell parks represent asynchronous workflow steps, compressed to 5 ms each. Steps/turn = 1/2/6 (small floor, p90/p99 calls/user turn); no real multi-day waits or owner crashes are claimed.',calls_per_turn=calls,delay_ms=5)
    fan=q('tau','tool_calls_session',p);b=q('tau','tool_output_bytes',p);inp=q('tau','tool_input_bytes',p)
    write('fanout',tier,'standard',[group(1,[dict(tool_calls=[tool(f'branch-{i}',b,inp) for i in range(fan)]),dict(text=fill(q('tau','model_output_bytes',p)))])],f'tau-bench {stats["tau"]["revision"]} p{p} session tool count {fan} collapsed into one parallel batch. Counterfactual adaptation: observed batches have width one, not {fan}. Tool input/output bytes use p{p}.',calls_per_turn=fan,output_bytes=b)
    n=q('swe','model_calls_session',p);b=q('swe','tool_output_bytes',p)
    write('reread',tier,'standard',[group(n,seq(1,b,q('swe','tool_input_bytes',p),key='same-file')+[dict(text='Rechecked the unchanged synthetic file {turn}.')])],f'SWE-agent-trajectories {stats["swe"]["revision"]} p{p} action counts and output bytes; only {q("swe","files_opened_session",p)} distinct opened-file paths versus {n} actions motivates repeated identical reads. One stable key isolates content reuse; it is not an observed read-frequency estimate.',calls_per_turn=1,output_bytes=b)
    n=q('swe','model_calls_session',p);normal=q('swe','tool_output_bytes',50);burst_p=90 if tier=='small' else 99;burst=q('swe','tool_output_bytes',burst_p)
    turns=[]
    if n>1:turns.append(group(n-1,seq(1,normal,32)+[dict(text='Synthetic small observation {turn}.')]))
    turns.append(group(1,seq(1,burst,32)+[dict(text='Synthetic output burst {turn}.')]))
    write('output-burst',tier,'standard',turns,f'SWE-agent-trajectories {stats["swe"]["revision"]} p{p} action count; ordinary results use p50 ({normal} bytes), final result p{burst_p} ({burst} bytes). Isolates tail output from typical turns; frequency and ordering are synthetic.',calls_per_turn=1,output_bytes=normal,burst_bytes=burst)
(out/'measurements/matrix.json').write_text(json.dumps(matrix,indent=2)+'\n')
