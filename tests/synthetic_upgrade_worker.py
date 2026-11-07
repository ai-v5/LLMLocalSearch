"""Exercise old/new workers against a task-owned synthetic upgrade fixture."""

import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time


def main() -> None:
    if os.name != "nt" or len(sys.argv) != 4:
        raise SystemExit("Expected a Windows worker, scenario directory, and mode")
    worker, scenario, mode = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve(), sys.argv[3]
    work = (Path(os.environ["RUNNER_TEMP"]) / "llmlocalsearch-legacy-upgrade").resolve()
    if (
        scenario.parent != work
        or worker.parent != scenario / "program"
        or worker.name != "index-worker.exe"
        or mode not in {"create", "migrate"}
        or not worker.is_file()
    ):
        raise SystemExit("Synthetic upgrade fixture is outside the task directory")

    files, database = scenario / "documents", scenario / "program/Index/index.db"
    expected_text = "\u5408\u6210\u7a97\u53e3\u9a8c\u8bc1 zq"
    if mode == "create":
        if files.exists() or database.exists():
            raise SystemExit("Synthetic fixture already exists")
        files.mkdir()
        database.parent.mkdir(exist_ok=True)
        (files / "synthetic.txt").write_text(expected_text, encoding="utf-8")
    elif not files.is_dir() or not database.is_file():
        raise SystemExit("Synthetic legacy index is missing")

    expected_version = 0 if mode == "create" else 2
    startup = subprocess.STARTUPINFO()
    startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startup.wShowWindow = 0
    child = subprocess.Popen(
        [str(worker), str(database), str(scenario / "program/runtime"), "0", "1", str(files)],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
        encoding="utf-8",
        startupinfo=startup,
        creationflags=0x08000000,
    )
    completed = False
    try:
        child.stdin.write("start\n")
        child.stdin.flush()
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if child.poll() is not None:
                raise RuntimeError("Synthetic index worker exited unexpectedly")
            try:
                with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=0.5) as db:
                    version = db.execute("PRAGMA user_version").fetchone()[0]
                    app_id = db.execute("PRAGMA application_id").fetchone()[0]
                    rows = db.execute(
                        "SELECT files.id,files.name,files.state,files.body "
                        "FROM terms JOIN files ON files.id=terms.rowid WHERE terms MATCH ?",
                        ("body_grams:(g5408x6210x7a97x AND g7ax71x)",),
                    ).fetchall()
                if app_id == 0x4C4C5358 and version == expected_version and rows == [
                    (1, "synthetic.txt", "indexed", expected_text)
                ]:
                    completed = True
                    break
            except sqlite3.Error:
                pass
            time.sleep(0.2)
        if not completed:
            raise RuntimeError("Synthetic text was not searchable after the expected index transition")
    finally:
        if child.poll() is None:
            try:
                child.stdin.write("stop\n")
                child.stdin.flush()
                child.wait(timeout=30)
            except (BrokenPipeError, OSError, subprocess.TimeoutExpired):
                child.kill()
                child.wait(timeout=15)
    if child.returncode != 0:
        raise RuntimeError("Synthetic index worker did not exit cleanly")
    print(f"Synthetic {mode}: schema {expected_version}, original body and search match PASS")


if __name__ == "__main__":
    main()
