# Killing the writer while it appends

The question: when the process that appends to a PST is killed at an arbitrary moment, does the file still open at a
committed state, the one before the batch it was writing or the one after it, and can the writer open it again and
go on?

- **Where and when:** pstsmith 0.1.0, 2026-11-08, macOS 15.6.1 on an Apple M4 Pro, on the startup disk (APFS).
- **The program that is killed:** [probe/](probe/), a small program on pstsmith's library. `append-probe append FILE N`
  opens FILE and appends N batches of 10 messages, each batch one commit, to Inbox, Probe/A and Probe/B in turn (a
  missing folder is created by the first batch that needs it, in its own commit); every third message has an
  attachment of up to 160 KB. It prints `ok N` when the N-th batch's commit has returned.
- **The kills:** [run_kills.py](run_kills.py) starts the probe and sends it SIGKILL after a random delay of 0 to 150
  ms (seed 20261108). Every 10 kills start on a new file; in between, the probe that starts after a kill opens the
  file that kill left. 500 kills, on 50 files.

## What is checked after each kill

A copy of the file is checked as the kill left it:

- `pst-table-check` must pass: every folder's counters and every contents-table cell against the messages;
- the file must hold the messages of the batches the probe acknowledged, or those and the batch it was writing (the
  kill can come after that batch's commit point and before `ok N` is printed);
- `pst-ref-check` (block reference counts and the allocation map against the B-trees) must pass if the header marks
  the allocation map valid.

Then the writer opens the copy again and appends one more batch, and both checks must pass, with 10 more messages and
the map marked valid.

The map check depends on the header because of how a commit ends. The header written at the commit point marks the
allocation map invalid; the writer then writes the map pages and marks the map valid in the header ("Commit
semantics" in `crates/pstsmith/src/session.rs`). A kill between the two leaves a committed file whose map is marked
invalid. MS-PST 2.6.1.3.7 says that such a map must be rebuilt before space is next allocated, that a writer which
cannot rebuild it must not change the file, and that a read-only reader may ignore the flag; pstsmith's writer
rebuilds it when it opens the file. `pst-ref-check` reads the map pages, so on such a file it reports a stale map
(exit status 1) or a map page that was not written yet (exit status 2, `AMap page N has the wrong page type`).

## Result

500 of 500 kills passed (`summary.txt`; `kills.jsonl` has one line per kill). Before the kills, the probes had
acknowledged 2,165 batches in all; the largest file reached 27 MB and 700 messages.

| As the kill left the file | Kills |
| --- | --- |
| the messages of the acknowledged batches, map marked valid; both checks passed as it was | 183 |
| those and the batch being written, map marked invalid: the kill came after that batch's commit point, before the map was marked valid | 288 |
| the messages of the acknowledged batches, map marked invalid: the kill came after the commit point of a folder's creation, which adds no message, or before the next run had rebuilt a map an earlier kill left invalid | 29 |

No file held anything else, and all 500 opened in the writer, took one more batch and passed both checks.

## What this does not show

- It kills a process. The operating system keeps what the process wrote, so a kill tests the order of the writer's
  writes, not whether they reach the disk. A power cut also loses what was not yet on the disk; whether the writer's
  `sync_data` calls are enough for that was not tested.
- One machine, one file system.
- These are appends through the library. `pstsmith convert` killed with SIGKILL leaves its partial output and its
  staging directory behind (see "Known limits" in [VERIFICATION.md](../../../VERIFICATION.md)).

## Repeat it

From the repository root. The run above used the code of tag v0.1.0; this folder was added after the tag and changes
no code.

    cargo build --workspace --release --locked
    cargo build --release --manifest-path docs/verification/0.1.0/crash-safety/probe/Cargo.toml \
        --target-dir target/probe
    python3 -I docs/verification/0.1.0/crash-safety/run_kills.py target/probe/release/append-probe target/release \
        /tmp/kills 500 20261108 > kills.jsonl 2> summary.txt

The random delays repeat with the seed; where each kill lands depends on the machine.
