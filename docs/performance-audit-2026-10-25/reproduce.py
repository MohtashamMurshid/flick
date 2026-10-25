#!/usr/bin/env python3
"""Reproduce the original audit with pinned source and generated local fixtures."""
from pathlib import Path
import argparse
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile

AUDITED_REVISION = "71587ecac24801d0d1eb3d67fe6dec9451034931"
BASELINE_INPUTS = (
    "src", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
    "scripts/make-test-video.sh",
)


def prepare_baseline(root, here, folder):
    """Export committed inputs without reading application files in the checkout."""
    try:
        revision = subprocess.check_output(
            ["git", "rev-parse", "--verify", f"{AUDITED_REVISION}^{{commit}}"],
            cwd=root, text=True, stderr=subprocess.PIPE,
        ).strip()
    except subprocess.CalledProcessError as error:
        raise ValueError(
            f"audited revision {AUDITED_REVISION} must be present in local Git "
            "history; use a checkout with that commit available (no automatic fetch)"
        ) from error
    if revision != AUDITED_REVISION:
        raise ValueError(f"expected audited revision {AUDITED_REVISION}, got {revision}")
    archive = subprocess.check_output(
        ["git", "archive", "--format=tar", revision, "--", *BASELINE_INPUTS],
        cwd=root, stderr=subprocess.PIPE,
    )
    source_root = folder / "audited-source"
    source_root.mkdir()
    # Copy regular blobs only; no archive links may point back into the checkout.
    with tarfile.open(fileobj=io.BytesIO(archive)) as snapshot:
        for entry in snapshot:
            destination = source_root / entry.name
            if not destination.resolve().is_relative_to(source_root.resolve()):
                raise ValueError(f"unsafe baseline archive path: {entry.name}")
            if entry.isdir():
                destination.mkdir(parents=True, exist_ok=True)
            elif entry.isfile():
                destination.parent.mkdir(parents=True, exist_ok=True)
                with snapshot.extractfile(entry) as blob:
                    destination.write_bytes(blob.read())
            else:
                raise ValueError(f"unsupported baseline archive entry: {entry.name}")

    harness, count = re.subn(
        r'#\[path = "\.\./\.\./(src/[^"]+)"\]',
        lambda match: '#[path = ' + json.dumps(
            str(source_root / match.group(1)), ensure_ascii=False,
        ) + ']',
        (here / "harness.rs").read_text(),
    )
    if count != 5 or "../../src/" in harness:
        raise ValueError("expected to pin all five audit harness source modules")
    (folder / "harness.rs").write_text(harness)
    manifest = (source_root / "Cargo.toml").read_text()
    manifest = manifest.replace('default-run = "flick"', "autobins = false")
    start, end = manifest.index("[[bin]]"), manifest.index("[dependencies]")
    manifest = manifest[:start] + (
        '[lib]\npath = ' + json.dumps(str(source_root / "src/lib.rs"), ensure_ascii=False)
        + '\n\n[[bin]]\nname = "flick-perf-audit"\npath = '
        + json.dumps(str(folder / "harness.rs"), ensure_ascii=False) + '\n\n'
    ) + manifest[end:]
    (folder / "Cargo.toml").write_text(manifest)
    shutil.copyfile(source_root / "Cargo.lock", folder / "Cargo.lock")
    return source_root


def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            f"Requires audited commit {AUDITED_REVISION} in local Git history. "
            "Inputs are automatically exported with git archive to a temporary "
            "directory; the current branch and dirty application files are never "
            "used. No checkout or fetch is performed. A full run also requires "
            "the pinned Rust toolchain, cached Cargo dependencies (offline build), "
            "FFmpeg with libx264/AAC, and libmpv. Current implementation timings "
            "belong in ../performance-implementation-2026-10-25/measure-library.py."
        ),
    )
    parser.add_argument("--runs", type=int, default=3, help="benchmark runs, 1–10 (default: 3)")
    parser.add_argument(
        "--prepare-only", action="store_true",
        help="export pinned inputs and rewrite a temporary harness; do not build or run fixtures",
    )
    args = parser.parse_args()
    if not 1 <= args.runs <= 10:
        parser.error("--runs must be between 1 and 10")
    here = Path(__file__).resolve().parent
    root = here.parent.parent
    folder = Path(tempfile.mkdtemp(prefix="flick-performance-audit-"))
    try:
        source_root = prepare_baseline(root, here, folder)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    if args.prepare_only:
        print(folder / "Cargo.toml")
        return
    env = dict(os.environ, CARGO_TARGET_DIR=str(folder / "target"), FLICK_AUDIO="null")
    subprocess.run(["bash", str(source_root / "scripts/make-test-video.sh"),
                    str(folder / "fixture.mp4")], cwd=source_root, check=True)
    subprocess.run(["cargo", "rustc", "--locked", "--offline", "--release", "--manifest-path",
                    str(folder / "Cargo.toml"), "--bin", "flick-perf-audit",
                    "--", "-C", "lto=off"], cwd=source_root, env=env, check=True)
    data = {
        "repo_head": AUDITED_REVISION,
        "cpu_cores": os.cpu_count(),
        "start_loadavg": os.getloadavg() if hasattr(os, "getloadavg") else None,
        "build": "release opt-level=3, bin LTO off; dependencies release; no desktop window",
        "fixture": "generated 12s 1280x720 30fps H264/AAC; BGRA target 1920x1080",
        "runs": [],
    }
    binary = folder / "target" / "release" / ("flick-perf-audit.exe" if os.name == "nt" else "flick-perf-audit")
    for index in range(args.runs):
        output = subprocess.check_output([str(binary), str(folder)], cwd=source_root, env=env,
                                         text=True, timeout=120)
        (folder / f"run-{index + 1}.jsonl").write_text(output)
        data["runs"].append([json.loads(line) for line in output.splitlines()])
    data["end_loadavg"] = os.getloadavg() if hasattr(os, "getloadavg") else None
    (folder / "measurements.json").write_text(json.dumps(data, indent=2) + "\n")
    print(folder / "measurements.json")


if __name__ == "__main__":
    main()
