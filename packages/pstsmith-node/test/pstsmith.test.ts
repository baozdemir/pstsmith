// Tests of the npm package against a real pstsmith binary: PSTSMITH_BINARY, or target/release/pstsmith of this repository.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, existsSync, readdirSync, rmSync, mkdirSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { convertMail, convertManifest, PstsmithError } from "../src/index.ts";

const exe = process.platform === "win32" ? "pstsmith.exe" : "pstsmith";
const binary = process.env.PSTSMITH_BINARY ?? resolve(import.meta.dirname, "../../../target/release", exe);

function dir(): string {
  return mkdtempSync(join(tmpdir(), "pstsmith-node-"));
}

const MBOX = "From a Mon Jan  1 10:00:00 2024\nFrom: a@example.invalid\nSubject: one\nDate: Mon, 01 Jan 2024 10:00:00 +0000\n\nfirst\n\n" +
  "From a Mon Jan  1 11:00:00 2024\nFrom: a@example.invalid\nSubject: two\n\nsecond\n";
const MANIFEST = [
  JSON.stringify({ schema: "pst-export-manifest/2", export_id: "node", item_count: 0, folder_count: 1 }),
  JSON.stringify({ folder_path: ["Inbox"] }),
].join("\n") + "\n";

test("converts_a_manifest", async () => {
  const d = dir();
  writeFileSync(join(d, "m.jsonl"), MANIFEST);
  const report = await convertManifest({ manifest: join(d, "m.jsonl"), output: { file: join(d, "out.pst") }, excluded: join(d, "x.jsonl"), binary });
  assert.equal(report.protocol, "pst-export-result/1");
  assert.equal(report.written, 0);
  assert.equal(report.engine.name, "pstsmith");
  assert.ok(existsSync(join(d, "out.pst")));
  rmSync(d, { recursive: true });
});

test("converts_an_mbox", async () => {
  const d = dir();
  writeFileSync(join(d, "Inbox.mbox"), MBOX);
  const report = await convertMail({ inputs: [join(d, "Inbox.mbox")], output: { file: join(d, "out.pst") }, binary });
  assert.equal(report.written, 2);
  assert.deepEqual(report.warnings, 0);
  assert.equal(report.output?.path, join(d, "out.pst"));
  rmSync(d, { recursive: true });
});

test("converts_into_parts", async () => {
  const d = dir();
  writeFileSync(join(d, "Inbox.mbox"), MBOX);
  const report = await convertMail({ inputs: [join(d, "Inbox.mbox")], output: { dir: join(d, "parts"), maxFileSize: "1GB" }, binary });
  assert.equal(report.outputs?.length, 1);
  assert.equal(report.file_size_limit?.bytes, 1_000_000_000);
  rmSync(d, { recursive: true });
});

test("refuses_existing_output_with_output_exists", async () => {
  const d = dir();
  writeFileSync(join(d, "Inbox.mbox"), MBOX);
  writeFileSync(join(d, "out.pst"), "keep");
  await assert.rejects(convertMail({ inputs: [join(d, "Inbox.mbox")], output: { file: join(d, "out.pst") }, binary }), (error: unknown) => {
    assert.ok(error instanceof PstsmithError);
    assert.equal(error.kind, "output-exists");
    assert.equal(error.exitCode, 4);
    return true;
  });
  rmSync(d, { recursive: true });
});

/// A stand-in binary that never finishes, to test abort and timeout without depending on how fast pstsmith is.
function slowBinary(d: string): string {
  if (process.platform === "win32") {
    const path = join(d, "slow.cmd");
    writeFileSync(path, "@echo off\r\nmkdir \"%~dp0parts-slow\"\r\nping -n 30 127.0.0.1 > nul\r\n");
    return path;
  }
  const path = join(d, "slow.sh");
  writeFileSync(path, "#!/bin/sh\nwhile [ \"$1\" != \"--output\" ] && [ $# -gt 0 ]; do shift; done\necho partial > \"$2\"\nsleep 30\n");
  chmodSync(path, 0o755);
  return path;
}

test("abort_kills_the_process_and_leaves_no_output", { skip: process.platform === "win32" }, async () => {
  const d = dir();
  const controller = new AbortController();
  const run = convertMail({ inputs: [join(d, "in.mbox")], output: { file: join(d, "out.pst") }, binary: slowBinary(d), signal: controller.signal });
  setTimeout(() => {
    assert.ok(existsSync(join(d, "out.pst")), "the stand-in wrote a partial output");
    controller.abort();
  }, 300);
  await assert.rejects(run, (error: unknown) => error instanceof PstsmithError && error.kind === "aborted");
  assert.ok(!existsSync(join(d, "out.pst")), "the partial output is removed");
  rmSync(d, { recursive: true });
});

test("timeout_kills_the_process", { skip: process.platform === "win32" }, async () => {
  const d = dir();
  const started = Date.now();
  await assert.rejects(convertMail({ inputs: [join(d, "in.mbox")], output: { file: join(d, "out.pst") }, binary: slowBinary(d), timeoutMs: 300 }),
    (error: unknown) => error instanceof PstsmithError && error.kind === "timeout");
  assert.ok(Date.now() - started < 10_000);
  assert.ok(!existsSync(join(d, "out.pst")));
  rmSync(d, { recursive: true });
});

test("unknown_result_field_is_refused", { skip: process.platform === "win32" }, async () => {
  const d = dir();
  const fake = join(d, "fake.sh");
  writeFileSync(fake, "#!/bin/sh\necho '{\"protocol\":\"pst-export-result/1\",\"engine\":{\"name\":\"x\",\"version\":\"0\"},\"manifest\":{\"schema\":\"s\",\"export_id\":\"e\",\"item_count\":0,\"sha256\":\"0\"},\"read\":0,\"written\":0,\"skipped\":0,\"batches\":0,\"unsupported\":{},\"output\":{\"path\":\"p\",\"bytes\":1},\"surprise\":1}'\n");
  chmodSync(fake, 0o755);
  await assert.rejects(convertMail({ inputs: [d], output: { file: join(d, "o.pst") }, binary: fake }),
    (error: unknown) => error instanceof PstsmithError && error.kind === "protocol-invalid" && /surprise/.test(error.detail));
  rmSync(d, { recursive: true });
});

test("stderr_and_stdout_are_capped", { skip: process.platform === "win32" }, async () => {
  const d = dir();
  const noisy = join(d, "noisy.sh");
  writeFileSync(noisy, "#!/bin/sh\nyes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -c 50000000\n");
  chmodSync(noisy, 0o755);
  await assert.rejects(convertMail({ inputs: [d], output: { file: join(d, "o.pst") }, binary: noisy }),
    (error: unknown) => error instanceof PstsmithError && error.kind === "protocol-invalid" && /too much output/.test(error.detail));
  rmSync(d, { recursive: true });
});

test("no_shell_is_used", async () => {
  const d = dir();
  const odd = join(d, "it's \"quoted\" $(touch pwned) & spaced");
  mkdirSync(odd);
  writeFileSync(join(odd, "Inbox.mbox"), MBOX);
  const report = await convertMail({ inputs: [join(odd, "Inbox.mbox")], output: { file: join(odd, "out; rm -rf x.pst") }, binary });
  assert.equal(report.written, 2);
  assert.ok(!existsSync(join(process.cwd(), "pwned")) && !readdirSync(d).includes("pwned"));
  rmSync(d, { recursive: true });
});

test("binary_override_is_used", async () => {
  const d = dir();
  await assert.rejects(convertMail({ inputs: [d], output: { file: join(d, "o.pst") }, binary: join(d, "missing-binary") }),
    (error: unknown) => error instanceof PstsmithError && error.kind === "binary-not-found");
  rmSync(d, { recursive: true });
});

test("an_existing_output_is_never_removed", { skip: process.platform === "win32" }, async () => {
  const d = dir();
  mkdirSync(join(d, "parts"));
  writeFileSync(join(d, "parts", "keep.txt"), "mine");
  const controller = new AbortController();
  controller.abort();
  const slow = slowBinary(d);
  for (const signal of [controller.signal, undefined]) {
    await assert.rejects(convertMail({ inputs: [join(d, "in.mbox")], output: { dir: join(d, "parts") }, binary: slow, signal, timeoutMs: 100 }),
      (error: unknown) => error instanceof PstsmithError && error.kind === "output-exists");
    assert.ok(existsSync(join(d, "parts", "keep.txt")), "the caller's directory stays");
  }
  rmSync(d, { recursive: true });
});

test("an_input_that_looks_like_an_option_is_an_input", async () => {
  const d = dir();
  writeFileSync(join(d, "-o.mbox"), MBOX);
  const cwd = process.cwd();
  process.chdir(d);
  try {
    const report = await convertMail({ inputs: ["-o.mbox"], output: { file: join(d, "out.pst") }, binary });
    assert.equal(report.written, 2);
  } finally {
    process.chdir(cwd);
  }
  rmSync(d, { recursive: true });
});
