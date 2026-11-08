"""Run pstsmith out of disk space on a small disk image, then check what it leaves. macOS only (hdiutil).

    python3 -I run_full_disk.py PSTSMITH APPEND_PROBE CHECK_BIN_DIR WORK_DIR > results.jsonl 2> summary.txt

Every run gets a fresh copy of an empty 16 MB HFS+ disk image, attached without Finder and removed after the run. A
filler file fills the volume and is then cut short, which leaves exactly the bytes it was cut by free. (A 16 MB APFS
volume keeps about 1 MB to itself and does not return the space at once, so the free space cannot be set on it.)

Appends: a file of 30 messages is copied onto the volume, the filler leaves between 0 and 1.2 MB free (100 steps), and
append-probe (../crash-safety/probe) appends batches of 10 messages until the disk is full. The file is checked as it
is left; then the filler is removed, the writer opens the file again and appends one more batch, and the file is
checked again.

Conversions: `pstsmith convert` of a 20 MB MBOX onto the volume with 1, 4 and 12 MB free, once with its staging
directory on another disk and once on the full volume. The conversion must fail with exit status 5 and leave nothing
behind, on the volume or in the staging directory.

One JSON line per run goes to stdout, the totals to stderr.
"""
import base64
import errno
import json
import os
import random
import shutil
import subprocess
import sys
import time

pstsmith, probe, check_dir, work = sys.argv[1:5]
PER_BATCH = 10  # messages per commit, as append-probe writes them
STEPS, STEP = 100, 12 * 1024
MiB = 1 << 20

os.makedirs(work, exist_ok=True)
empty, image, mnt = (os.path.join(work, name) for name in ("empty.dmg", "volume.dmg", "volume"))
filler = os.path.join(mnt, "filler")


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


def header(path):
    # ibFileEof and fAMapValid, in the ROOT of the Unicode HEADER (MS-PST 2.2.2.5, 2.2.2.6)
    with open(path, "rb") as file:
        data = file.read(0xF9)
    return int.from_bytes(data[0xB8:0xC0], "little"), data[0xF8] != 0


def free():
    stat = os.statvfs(mnt)
    return stat.f_bavail * stat.f_frsize


def attach():
    shutil.copyfile(empty, image)
    os.makedirs(mnt, exist_ok=True)
    subprocess.run(["hdiutil", "attach", "-nobrowse", "-mountpoint", mnt, image], check=True, capture_output=True)


def detach():
    # macOS may still hold the volume for a moment (Spotlight); try again, then force it.
    for _ in range(10):
        if subprocess.run(["hdiutil", "detach", mnt], capture_output=True).returncode == 0:
            break
        time.sleep(1)
    else:
        subprocess.run(["hdiutil", "detach", "-force", mnt], check=True, capture_output=True)
    os.remove(image)


def fill(leave):
    """Fill the volume with a filler file, then cut it short by `leave` bytes; returns the free bytes after it."""
    chunk = MiB
    with open(filler, "wb", buffering=0) as file:
        while chunk >= 4096:
            try:
                file.write(b"\0" * chunk)
            except OSError as error:
                if error.errno != errno.ENOSPC:
                    raise
                chunk //= 2
        file.truncate(max(os.fstat(file.fileno()).st_size - leave, 0))
        os.fsync(file.fileno())
    return free()


def mbox(path):
    """40 messages, each with a 360 KB attachment of random bytes (about 20 MB as MBOX)."""
    rng = random.Random(1)
    with open(path, "w", newline="\n") as file:
        for n in range(40):
            data = base64.encodebytes(rng.randbytes(360_000)).decode()
            file.write(
                f"From sender@example.com Mon Nov  3 10:00:00 2025\n"
                f"From: sender@example.com\nTo: to@example.com\nSubject: message {n}\n"
                f"Message-ID: <{n}@full-disk.example>\nDate: Mon, 3 Nov 2025 10:00:00 +0000\nMIME-Version: 1.0\n"
                f'Content-Type: multipart/mixed; boundary="b"\n\n--b\nContent-Type: text/plain\n\nbody {n}\n\n'
                f'--b\nContent-Type: application/octet-stream; name="data.bin"\nContent-Transfer-Encoding: base64\n'
                f'Content-Disposition: attachment; filename="data.bin"\n\n{data}--b--\n\n'
            )


def append_run(step, base, before):
    pst = os.path.join(mnt, "file.pst")
    shutil.copyfile(base, pst)
    free_bytes = fill(step * STEP)
    code, out, _ = run(probe, "append", pst, "100000")
    acked = sum(1 for line in out.splitlines() if line.startswith("ok "))
    error = next((line for line in out.splitlines() if line.startswith("error ")), "")
    eof, valid = header(pst)
    file_bytes = os.path.getsize(pst)
    table_code, count = table_check(pst)
    ref_code, ref_error = ref_check(pst)
    old, new = before + acked * PER_BATCH, before + (acked + 1) * PER_BATCH
    state = "old" if count == old else "new" if count == new else "neither"
    # As in ../crash-safety: pst-ref-check reads the map pages, so as the file is left it counts only when the header
    # marks the map valid; the writer that opens the file next rebuilds a map marked invalid (MS-PST 2.6.1.3.7).
    as_left = (
        code == 1
        and "(os error 28)" in error
        and table_code == 0
        and state != "neither"
        and (ref_code == 0 or not valid)
    )

    # Cut to nothing before the unlink: the space comes back at once even if another process (Spotlight) holds it open.
    os.truncate(filler, 0)
    os.remove(filler)
    free_for_reopen = free()
    reopen_code, reopen_out, _ = run(probe, "append", pst, "1")
    table_again, count_again = table_check(pst)
    ref_again = ref_check(pst)[0]
    reopened = (
        reopen_code == 0
        and table_again == 0
        and ref_again == 0
        and count is not None
        and count_again == count + PER_BATCH
        and header(pst)[1]
    )
    return {
        "part": "append", "step": step, "free_bytes": free_bytes, "probe_exit": code, "error": error,
        "batches_acknowledged": acked, "messages_before_run": before, "messages": count, "state": state,
        "file_bytes": file_bytes, "header_eof": eof, "map_marked_valid": valid, "table_check_exit": table_code,
        "ref_check_exit": ref_code, "ref_check_error": ref_error, "checks_as_left": as_left,
        "free_bytes_for_reopen": free_for_reopen, "reopen_append_exit": reopen_code, "reopen_append_output": reopen_out.strip(),
        "messages_after_reopen": count_again, "file_bytes_after_reopen": os.path.getsize(pst),
        "header_eof_after_reopen": header(pst)[0], "table_check_exit_after_reopen": table_again,
        "ref_check_exit_after_reopen": ref_again, "checks_after_reopen": reopened, "passed": as_left and reopened,
    }


def convert_run(source, leave, where):
    staging = os.path.join(work, "staging") if where == "other-disk" else os.path.join(mnt, "staging")
    shutil.rmtree(staging, ignore_errors=True)
    os.makedirs(staging)
    free_bytes = fill(leave)
    code, _, error = run(pstsmith, "convert", source, "-o", os.path.join(mnt, "out.pst"), "--temp-dir", staging)
    left = sorted(name for name in os.listdir(mnt) if not name.startswith(".") and name not in ("filler", "staging"))
    staged = sorted(os.listdir(staging))
    shutil.rmtree(staging, ignore_errors=True)
    return {
        "part": "convert", "free_bytes": free_bytes, "staging": where, "exit": code,
        "error": error.splitlines()[0] if error else "", "left_on_volume": left, "left_in_staging": staged,
        "passed": code == 5 and not left and not staged,
    }


subprocess.run(["hdiutil", "create", "-ov", "-size", "16m", "-fs", "HFS+", "-volname", "pstsmith-full", empty],
               check=True, capture_output=True)
base, source = os.path.join(work, "base.pst"), os.path.join(work, "input.mbox")
if os.path.exists(base):
    os.remove(base)
assert run(probe, "create", base)[0] == 0 and run(probe, "append", base, "3")[0] == 0
before = table_check(base)[1]
mbox(source)

runs = [("append", step) for step in range(STEPS)]
runs += [("convert", (leave, where)) for leave in (1 * MiB, 4 * MiB, 12 * MiB) for where in ("other-disk", "full-volume")]
totals = {"append_runs": 0, "append_passed": 0, "convert_runs": 0, "convert_passed": 0}
for part, parameters in runs:
    attach()
    try:
        if part == "append":
            result = append_run(parameters, base, before)
        else:
            result = convert_run(source, *parameters)
    finally:
        detach()
    print(json.dumps(result), flush=True)
    totals[part + "_runs"] += 1
    totals[part + "_passed"] += result["passed"]

os.remove(empty)
print(json.dumps(totals), file=sys.stderr)
sys.exit(0 if totals["append_passed"] == STEPS and totals["convert_passed"] == totals["convert_runs"] else 1)
