"""Kill append-probe with SIGKILL at random moments while it appends, then check the file it leaves.

    python3 -I run_kills.py PROBE CHECK_BIN_DIR WORK_DIR KILLS SEED > kills.jsonl 2> summary.txt

Every 10 kills start from a new file; in between, each run opens the file the previous kill left. After each kill a
copy of the file is checked as it is, then opened again with the writer, given one more batch and checked again.
One JSON line per kill goes to stdout, the totals to stderr.
"""
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import time

probe, check_dir, work, kills, seed = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
CHAIN = 10  # kills on one file
PER_BATCH = 10  # messages per commit, as append-probe writes them
MAX_DELAY = 0.15  # seconds: spans the probe's start and several of its commits on the machine of this run

rng = random.Random(seed)
os.makedirs(work, exist_ok=True)
pst, copy, out_path = (os.path.join(work, name) for name in ("file.pst", "copy.pst", "probe.out"))


def run(*args):
    done = subprocess.run(args, capture_output=True, text=True)
    return done.returncode, done.stdout, done.stderr.strip()


def table_check(path):
    code, out, _ = run(os.path.join(check_dir, "pst-table-check"), path)
    count = next((int(line.split()[1]) for line in out.splitlines() if line.startswith("MESSAGES ")), None)
    return code, count


def ref_check(path):
    code, _, error = run(os.path.join(check_dir, "pst-ref-check"), path)
    return code, error


def amap_marked_valid(path):
    # fAMapValid, in the ROOT of the Unicode HEADER (MS-PST 2.2.2.5, 2.2.2.6)
    with open(path, "rb") as file:
        file.seek(0xF8)
        return file.read(1)[0] != 0


totals = {"kills": 0, "passed": 0, "old_state": 0, "new_state": 0, "map_marked_invalid": 0, "reopened": 0}
before = 0
for kill in range(1, kills + 1):
    if (kill - 1) % CHAIN == 0:
        if os.path.exists(pst):
            os.remove(pst)
        assert run(probe, "create", pst)[0] == 0
        before = 0
    delay = rng.uniform(0, MAX_DELAY)
    with open(out_path, "w") as out:
        process = subprocess.Popen([probe, "append", pst, "100000"], stdout=out)
        time.sleep(delay)
        process.send_signal(signal.SIGKILL)
        exit_status = process.wait()
    with open(out_path) as out:
        acked = sum(1 for line in out if line.startswith("ok "))
    shutil.copyfile(pst, copy)
    old, new = before + acked * PER_BATCH, before + (acked + 1) * PER_BATCH

    valid = amap_marked_valid(copy)
    table_code, count = table_check(copy)
    ref_code, ref_error = ref_check(copy)
    state = "old" if count == old else "new" if count == new else "neither"
    # A map the header marks invalid is rebuilt by the next writer that opens the file (MS-PST 2.6.1.3.7). pst-ref-check
    # reads the map pages, so as the file is left it counts only when the header marks the map valid; after the reopen
    # it always counts.
    as_left = exit_status == -signal.SIGKILL and table_code == 0 and state != "neither" and (ref_code == 0 or not valid)

    reopen_code = run(probe, "append", copy, "1")[0]
    table_again, count_again = table_check(copy)
    ref_again = ref_check(copy)[0]
    reopened = (
        reopen_code == 0
        and table_again == 0
        and ref_again == 0
        and count is not None
        and count_again == count + PER_BATCH
        and amap_marked_valid(copy)
    )

    print(
        json.dumps(
            {
                "kill": kill,
                "delay_ms": round(delay * 1000, 1),
                "exit_status": exit_status,
                "batches_acknowledged": acked,
                "file_bytes": os.path.getsize(pst),
                "messages_before_run": before,
                "messages": count,
                "state": state,
                "map_marked_valid": valid,
                "table_check_exit": table_code,
                "ref_check_exit": ref_code,
                "ref_check_error": ref_error,
                "checks_as_left": as_left,
                "reopen_append_exit": reopen_code,
                "messages_after_reopen": count_again,
                "table_check_exit_after_reopen": table_again,
                "ref_check_exit_after_reopen": ref_again,
                "checks_after_reopen": reopened,
            }
        ),
        flush=True,
    )
    totals["kills"] += 1
    totals["passed"] += as_left and reopened
    totals["old_state"] += state == "old"
    totals["new_state"] += state == "new"
    totals["map_marked_invalid"] += not valid
    totals["reopened"] += reopened
    before = count if count is not None else before

print(json.dumps(totals), file=sys.stderr)
sys.exit(0 if totals["passed"] == totals["kills"] else 1)
