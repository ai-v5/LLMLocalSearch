"""Build the fixed synthetic corpus index; never point this at user files.

Run the synthetic performance workflow manually, or push a commit with
`[bench]` in its message. The worker runs directly on an isolated machine,
without the desktop's Windows Job limit. This measures index layout and query
readiness, not interactive latency or production indexing throughput.
"""

import json
import os
from pathlib import Path
import subprocess
import threading
import time

root = Path(__file__).resolve().parent.parent
cache = (root / ".cache").resolve()
corpus = (cache / "benchmark-corpus").resolve()
output = (cache / "benchmark-index").resolve()
manifest = cache / "benchmark-corpus-manifest.json"
expected_files = 100_000
expected_bytes = expected_files * 10_738
if (
    cache.parent != root
    or corpus.parent != cache
    or output.parent != cache
    or not corpus.is_dir()
    or not manifest.is_file()
    or output.exists()
):
    raise SystemExit("Synthetic benchmark directories are missing or already exist")
if json.loads(manifest.read_text(encoding="utf-8")) != {
    "schema": 1, "seed": 0x5EED, "files": expected_files, "bytes_per_file": 10_738
}:
    raise SystemExit("Synthetic benchmark manifest does not match the fixed workload")
paths = [path for path in corpus.rglob("*") if path.is_file()]
if (
    len(paths) != expected_files
    or any(path.suffix != ".txt" for path in paths)
    or sum(path.stat().st_size for path in paths) != expected_bytes
):
    raise SystemExit("Synthetic corpus size does not match the fixed 100,000-file workload")
executable = root / "target/release/index-worker.exe"
if os.name != "nt" or not executable.is_file():
    raise SystemExit("The Windows release index worker is required")
output.mkdir()
database = output / "index.db"

startup = subprocess.STARTUPINFO()
startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
startup.wShowWindow = 0
child = subprocess.Popen(
    [str(executable), str(database), str(cache / "no-runtime"), "0", "1", str(corpus)],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    text=True,
    encoding="utf-8",
    startupinfo=startup,
    creationflags=0x08000000,
)
snapshot = {}
snapshot_lock = threading.Lock()


def read_status():
    for line in child.stdout:
        try:
            value = json.loads(line)
        except (TypeError, ValueError):
            continue
        if isinstance(value, dict) and "files" in value:
            with snapshot_lock:
                snapshot.clear()
                snapshot.update(value)


reader = threading.Thread(target=read_status, daemon=True)
reader.start()
start = time.monotonic()
deadline = start + 165 * 60
next_report = start
completed = False
try:
    child.stdin.write("start\n")
    child.stdin.flush()
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise RuntimeError(f"Synthetic worker exited; code={child.returncode}")
        with snapshot_lock:
            current = dict(snapshot)
        files = current.get("files", 0)
        indexed = current.get("indexed", 0)
        failed = current.get("failed", 0)
        pending = current.get("pending", 0)
        if failed:
            raise RuntimeError("At least one synthetic document failed to index")
        if time.monotonic() >= next_report:
            print(
                f"synthetic_files={files}/{expected_files} indexed={indexed} "
                f"pending={pending} elapsed_s={time.monotonic()-start:.0f}",
                flush=True,
            )
            next_report = time.monotonic() + 60
        if files == expected_files and indexed == expected_files and pending == 0:
            completed = True
            break
        time.sleep(2)
    else:
        raise RuntimeError("The full synthetic indexing workload did not finish within the test deadline")
finally:
    if child.poll() is None:
        try:
            child.stdin.write("stop\n")
            child.stdin.flush()
            child.wait(timeout=30)
        except (BrokenPipeError, OSError, subprocess.TimeoutExpired):
            child.kill()
            child.wait(timeout=15)
    reader.join(timeout=5)

if not completed or child.returncode != 0:
    raise RuntimeError(f"Synthetic worker did not stop cleanly; code={child.returncode}")

sizes = [database, Path(str(database) + "-wal"), Path(str(database) + "-shm")]
total = sum(path.stat().st_size for path in sizes if path.exists())
print(
    f"synthetic_index_complete files={expected_files} source_bytes={expected_bytes} "
    f"index_bytes={total}",
    flush=True,
)
