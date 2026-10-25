#!/usr/bin/env python3
"""Reproduce audit helper timings with local fixtures; makes no app changes."""
from pathlib import Path
import argparse
import json
import os
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--runs", type=int, default=3)
args = parser.parse_args()
if not 1 <= args.runs <= 10:
    parser.error("--runs must be between 1 and 10")
here = Path(__file__).resolve().parent
root = here.parent.parent
folder = Path(tempfile.mkdtemp(prefix="flick-performance-audit-"))
manifest = (root / "Cargo.toml").read_text()
manifest = manifest.replace('default-run = "flick"', "autobins = false")
start, end = manifest.index("[[bin]]"), manifest.index("[dependencies]")
manifest = manifest[:start] + (
    f'[lib]\npath = "{root}/src/lib.rs"\n\n'
    f'[[bin]]\nname = "flick-perf-audit"\npath = "{here}/harness.rs"\n\n'
) + manifest[end:]
(folder / "Cargo.toml").write_text(manifest)
shutil.copyfile(root / "Cargo.lock", folder / "Cargo.lock")
env = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"), FLICK_AUDIO="null")
subprocess.run(["bash", str(root / "scripts/make-test-video.sh"),
                str(folder / "fixture.mp4")], cwd=root, check=True)
subprocess.run(["cargo", "rustc", "--locked", "--release", "--manifest-path",
                str(folder / "Cargo.toml"), "--bin", "flick-perf-audit",
                "--", "-C", "lto=off"], cwd=root, env=env, check=True)
data = {
    "repo_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
    "cpu_cores": os.cpu_count(),
    "start_loadavg": os.getloadavg() if hasattr(os, "getloadavg") else None,
    "build": "release opt-level=3, bin LTO off; dependencies release; no desktop window",
    "fixture": "generated 12s 1280x720 30fps H264/AAC; BGRA target 1920x1080",
    "runs": [],
}
binary = root / "target" / "release" / ("flick-perf-audit.exe" if os.name == "nt" else "flick-perf-audit")
for index in range(args.runs):
    output = subprocess.check_output([str(binary), str(folder)], cwd=root, env=env,
                                     text=True, timeout=120)
    (folder / f"run-{index + 1}.jsonl").write_text(output)
    data["runs"].append([json.loads(line) for line in output.splitlines()])
data["end_loadavg"] = os.getloadavg() if hasattr(os, "getloadavg") else None
(folder / "measurements.json").write_text(json.dumps(data, indent=2) + "\n")
print(folder / "measurements.json")
