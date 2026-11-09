#!/usr/bin/env python3
# +-------------------------------------------------------------------------
# Taskmgr bounded state-machine verification and real-source mutation checks
# File: verification/formal/verify.py
# Date: 2026-11-09; environment: Darwin arm64, Python 3.9.6, Java 25.0.4.1
# Author: OpenAI Codex
# --------------------------------------------------------------------------
"""Run TLC on finite models and replay their critical paths in extracted Rust.

A model pass does not prove the full Win32 program. A drift error is not a bug
verdict. All generated Rust, mutations, Java temp files and logs stay in --out.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from source_probe import MUTATIONS, generate

ROOT=Path(__file__).resolve().parents[2]
MODEL_FILE=Path(__file__).with_name('models.json')
JAR_SHA256='936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88'

def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()

def invoke(args,cwd,timeout,log):
    result=subprocess.run([str(a) for a in args],cwd=cwd,text=True,capture_output=True,timeout=timeout)
    log.write_text(result.stdout+result.stderr)
    return result

def check_model(area,m,out,jar,java,broken=False):
    dest=out/area/('mutant-model' if broken else 'model')
    dest.mkdir(parents=True)
    folder=ROOT/(m.get('model_dir') or m['skill'])
    module=m['module']
    for suffix in ['.tla','.cfg','-broken.cfg']:
        shutil.copy2(folder/(module+suffix),dest/(module+suffix))
    (dest/'java-tmp').mkdir()
    config=module+('-broken.cfg' if broken else '.cfg')
    result=invoke([java,'-Xmx256m','-XX:+UseParallelGC','-Djava.io.tmpdir='+str(dest/'java-tmp'),
        '-cp',jar,'tlc2.TLC','-workers','1','-seed','1','-fp','0','-config',config,module+'.tla'],dest,60,dest/'tlc.out')
    text=result.stdout+result.stderr
    if result.returncode==0 and 'Model checking completed. No error has been found.' in text:
        verdict='pass'
    elif 'Invariant Safety is violated.' in text:
        verdict='violation'
    else: raise RuntimeError('TLC did not finish checking '+area+': '+str(dest/'tlc.out'))
    return {'verdict':verdict,'exit_code':result.returncode,'log':str(dest/'tlc.out')}

def source_probe(area,m,out,source_root,mutant=False):
    dest=out/area/('mutant-source' if mutant else 'source')
    dest.mkdir(parents=True,exist_ok=True)
    if mutant:
        shadow=dest/'tree'
        for record in m['coverage']:
            path=record['path']; target=shadow/path
            target.parent.mkdir(parents=True,exist_ok=True)
            shutil.copy2(ROOT/path,target)
        for path,old,new in MUTATIONS[area]:
            file=shadow/path
            text=file.read_text()
            if text.count(old)!=1: raise RuntimeError('Mutation anchor drift: '+path)
            file.write_text(text.replace(old,new,1))
        source_root=shadow
    rust=dest/'probe.rs'; rust.write_text(generate(source_root,area))
    binary=dest/('probe.exe' if os.name=='nt' else 'probe')
    build=invoke(['rustc','--edition=2024',rust,'-o',binary],dest,60,dest/'compile.log')
    if build.returncode: raise RuntimeError('Probe did not compile: '+str(dest/'compile.log'))
    result=invoke([binary],dest,10,dest/'replay.log')
    if mutant and result.returncode==0: raise RuntimeError('Source mutation was not detected: '+area)
    expected=1 if mutant else 0
    if bool(result.returncode)!=bool(expected):
        raise RuntimeError('Unexpected source replay outcome: '+str(dest/'replay.log'))
    return {'verdict':'assertion_failed' if result.returncode else 'pass','exit_code':result.returncode,'log':str(dest/'replay.log')}

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('area',nargs='?',default='all')
    parser.add_argument('--self-test',action='store_true',help='also break each property in scratch copies of real Rust and TLA+')
    parser.add_argument('--out',type=Path,help='new or empty folder outside the repository')
    parser.add_argument('--jar',type=Path,default=Path(os.getenv('TLA2TOOLS',str(ROOT/'verification/.tools/tla2tools-1.7.4.jar'))))
    parser.add_argument('--java',default=os.getenv('JAVA','java'))
    args=parser.parse_args()
    if args.area=='applications_force': args.area='applications'  # prior CLI alias
    models=json.loads(MODEL_FILE.read_text())
    if args.area!='all': models={args.area:models[args.area]}
    for area,m in models.items():
        for record in m['coverage']:
            file=ROOT/record['path']
            if not file.exists() or sha(file)!=record['sha256']:
                raise RuntimeError('SOURCE DRIFT, not an invariant verdict: '+record['path']+'. Read the subsystem verification notes, compare code to model, then update the sync record only after review.')
    if not args.jar.exists() or sha(args.jar)!=JAR_SHA256:
        raise RuntimeError('Need verified TLA+ tools 1.7.4. Set TLA2TOOLS or --jar. Expected SHA-256 '+JAR_SHA256)
    out=(args.out or Path(tempfile.mkdtemp(prefix='taskmgr-formal-'))).resolve()
    if ROOT==out or ROOT in out.parents: raise RuntimeError('--out must be outside the repository')
    if out.exists() and any(out.iterdir()): raise RuntimeError('--out must be new or empty')
    out.mkdir(parents=True,exist_ok=True)
    results={}
    for area,m in models.items():
        record={'model':check_model(area,m,out,args.jar.resolve(),args.java),
                'source':source_probe(area,m,out,ROOT)}
        expected='violation' if m['expected_tlc_exit'] else 'pass'
        if record['model']['verdict']!=expected:
            raise RuntimeError('Changed model verdict for '+area+'; inspect '+str(out/area))
        if args.self_test:
            record['mutation_model']=check_model(area,m,out,args.jar.resolve(),args.java,True)
            if record['mutation_model']['verdict']!='violation': raise RuntimeError('TLA mutation was not detected: '+area)
            record['mutation_source']=source_probe(area,m,out,ROOT,True)
            record['restored_model']=check_model(area,m,out/'restored',args.jar.resolve(),args.java)
            record['restored_source']=source_probe(area,m,out/'restored',ROOT)
            if record['restored_model']['verdict']!=expected:
                raise RuntimeError('Original model did not return to its baseline: '+area)
        results[area]=record
        print(area+': '+record['model']['verdict']+'; real-source replay '+record['source']['verdict']+
              ('; both negative controls detected' if args.self_test else ''),flush=True)
    report={'sync_commit':next(iter(models.values()))['sync_commit'],'tool':'tla2tools 1.7.4 / TLC 2.19',
            'scope':'finite models plus controlled Rust replays; not whole-program verification','results':results}
    (out/'summary.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    print('Report: '+str(out/'summary.json'))
    return 1 if any(r['model']['verdict']=='violation' for r in results.values()) else 0

if __name__=='__main__':
    try: sys.exit(main())
    except (OSError,RuntimeError,ValueError,KeyError,subprocess.TimeoutExpired) as exc:
        print(str(exc),file=sys.stderr); sys.exit(2)
