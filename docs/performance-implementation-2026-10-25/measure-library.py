#!/usr/bin/env python3
"""Measure production library APIs with generated records; no network or desktop."""
from pathlib import Path
import json, os, shutil, subprocess, tempfile
here=Path(__file__).resolve().parent
root=here.parent.parent
folder=Path(tempfile.mkdtemp(prefix='flick-performance-after-'))
manifest=(root/'Cargo.toml').read_text().replace('default-run = "flick"','autobins = false')
a,b=manifest.index('[[bin]]'),manifest.index('[dependencies]')
manifest=manifest[:a]+f'[lib]\npath = "{root}/src/lib.rs"\n\n[[bin]]\nname = "flick-perf-after"\npath = "{here}/library-harness.rs"\n\n'+manifest[b:]
(folder/'Cargo.toml').write_text(manifest)
shutil.copyfile(root/'Cargo.lock',folder/'Cargo.lock')
env=dict(os.environ,CARGO_TARGET_DIR=str(root/'target'))
subprocess.run(['cargo','rustc','--locked','--release','--manifest-path',str(folder/'Cargo.toml'),'--bin','flick-perf-after','--','-C','lto=off'],cwd=root,env=env,check=True)
data={'build':'release opt-level=3, bin LTO off; no desktop window','start_loadavg':os.getloadavg(),'runs':[]}
for i in range(3):
    output=subprocess.check_output([str(root/'target/release/flick-perf-after'),str(folder)],cwd=root,env=env,text=True,timeout=120)
    data['runs'].append([json.loads(line) for line in output.splitlines()])
data['end_loadavg']=os.getloadavg()
(here/'measurements.json').write_text(json.dumps(data,indent=2)+'\n')
print(here/'measurements.json')
