#!/usr/bin/env python3
"""run_suite.py — run every *.py test in a folder and aggregate pass/fail.

Launched by PcanWork's "运行套件 / Run folder" button (folder passed via env
PCANWORK_SUITE_DIR). Can also be run directly:  python run_suite.py [FOLDER]
(defaults to this file's own folder).

Each test runs as its OWN process with the IPC env inherited, so a crash or
sys.exit in one test can't sink the suite. Output streams live into the runner
pane. Exit code 0 iff every test passed (non-zero exit = that test failed).

Excluded from the run: this file, pcanwork.py, and any file starting with "_".
Per-test timeout: PCANWORK_TEST_TIMEOUT seconds (default 120).
"""
import os
import sys
import glob
import time
import subprocess

PER_TEST_TIMEOUT = float(os.environ.get("PCANWORK_TEST_TIMEOUT", "120"))


def main() -> int:
    folder = (sys.argv[1] if len(sys.argv) > 1
              else os.environ.get("PCANWORK_SUITE_DIR")
              or os.path.dirname(os.path.abspath(__file__)))
    folder = os.path.abspath(folder)
    me = os.path.basename(__file__).lower()
    tests = sorted(
        f for f in glob.glob(os.path.join(folder, "*.py"))
        if os.path.basename(f).lower() not in (me, "pcanwork.py")
        and not os.path.basename(f).startswith("_"))

    if not tests:
        print(f"[suite] no runnable *.py tests in: {folder}")
        return 1

    print(f"[suite] {len(tests)} test(s) in {folder}\n")
    results = []  # (name, passed, seconds)
    for i, path in enumerate(tests, 1):
        name = os.path.basename(path)
        print(f"\n━━ [{i}/{len(tests)}] {name} ━━")
        t0 = time.monotonic()
        try:
            r = subprocess.run([sys.executable, path], timeout=PER_TEST_TIMEOUT)
            passed, code = (r.returncode == 0), r.returncode
        except subprocess.TimeoutExpired:
            print(f"[suite] TIMEOUT after {PER_TEST_TIMEOUT:.0f}s — killed")
            passed, code = False, -1
        except Exception as e:  # noqa: BLE001 — keep the suite alive
            print(f"[suite] could not launch: {e}")
            passed, code = False, -1
        dt = time.monotonic() - t0
        print(f"  → {'PASS' if passed else 'FAIL'} (exit {code}, {dt:.1f}s)")
        results.append((name, passed, dt))
        time.sleep(0.25)  # let the app release the single-run gate before next

    npass = sum(1 for _, p, _ in results if p)
    total = time.monotonic()
    print("\n" + "═" * 56)
    print(f"SUITE RESULT: {npass}/{len(results)} passed")
    for name, p, dt in results:
        mark = "✓" if p else "✗"
        print(f"  {mark} {name:<34} {dt:6.1f}s")
    print("═" * 56)
    return 0 if npass == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
