"""Generate the fixed 100,000-file, >=1 GiB synthetic performance corpus."""
import pathlib
import random
import os
import sys
import time
import json

if os.name == "nt":
    import ctypes
    kernel = ctypes.windll.kernel32
    kernel.GetCurrentProcess.restype = ctypes.c_void_p
    kernel.SetPriorityClass.argtypes = (ctypes.c_void_p, ctypes.c_uint32)
    kernel.SetPriorityClass(kernel.GetCurrentProcess(), 0x00100000)

root = pathlib.Path(__file__).resolve().parent.parent
cache = (root / ".cache").resolve()
destination = (cache / "benchmark-corpus").resolve()
manifest = cache / "benchmark-corpus-manifest.json"
if cache.parent != root or destination.parent != cache or destination.exists() or manifest.exists():
    raise SystemExit("Benchmark destination already exists or is outside the task cache")
cache.mkdir(exist_ok=True)
destination.mkdir()
randomizer = random.Random(0x5EED)
alphabet = "本地搜索文件内容性能系统模型数据文档索引排序后台测试电脑时间目录状态更新样例"
templates = []
for _ in range(128):
    words = "".join(randomizer.choice(alphabet) for _ in range(2400))
    payload = ("固定短语锚点 ExampleBenchmark " + words).encode("utf-8")
    templates.append(payload)
size = 10_738
start = time.monotonic()
for i in range(100_000):
    if i % 1_000 == 0:
        folder = destination / f"group-{i // 1000:03d}"
        folder.mkdir()
    header = f"SYNTHETIC-{i:06d} ".encode("ascii")
    base = templates[i % len(templates)]
    repeated = (base * ((size // len(base)) + 1))[:size - len(header)]
    # Do not split a UTF-8 sequence at the fixed byte boundary.
    text = repeated.decode("utf-8", errors="ignore").encode("utf-8")
    data = header + text + b" " * (size - len(header) - len(text))
    (folder / f"document-{i:06d}.txt").write_bytes(data)
    if (i + 1) % 10_000 == 0:
        print(f"generated={i + 1} bytes={(i + 1) * size} elapsed={time.monotonic() - start:.1f}s", flush=True)
with manifest.open("x", encoding="utf-8") as output:
    json.dump({"schema": 1, "seed": 0x5EED, "files": 100_000, "bytes_per_file": size}, output)
print(f"complete files=100000 bytes={100000 * size}", flush=True)
