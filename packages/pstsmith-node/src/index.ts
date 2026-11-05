// pstsmith for Node.js: a typed, Promise-based API over the pstsmith binary, which runs as a child process. A crash or
// a memory blow-up of a conversion never takes down the calling process, and an AbortSignal or a timeout really stops
// it: the process is killed and what it wrote is removed.
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { resolveBinary } from "./binary.ts";
import { parseError, parseResult, ProtocolError, type Report } from "./protocol.ts";

export type { Engine, OutputFile, Report } from "./protocol.ts";

/** Where a conversion writes: one file, or a new directory of size-limited parts. The path must not exist yet. */
export type Output = { file: string } | { dir: string; maxFileSize?: "1GB" | "2GB" | "5GB" | "10GB" };

export interface CommonOptions {
  output: Output;
  /** Record items that cannot be written faithfully here instead of failing (the path must not exist yet). */
  excluded?: string;
  /** Stops the conversion: the process is killed and its outputs removed. */
  signal?: AbortSignal;
  /** Stops the conversion after this many milliseconds, as `signal` does. */
  timeoutMs?: number;
  /** The pstsmith binary; defaults to PSTSMITH_BINARY, then the installed platform package. */
  binary?: string;
  /** Items per commit. */
  batch?: number;
  /** Body and HTML bytes per commit. */
  batchBytes?: number;
}

export interface ManifestOptions extends CommonOptions {
  /** A JSONL manifest (`pst-export-manifest/1` or `/2`). */
  manifest: string;
}

export interface MailOptions extends CommonOptions {
  /** EML files, MBOX files, Maildir directories, Thunderbird or Apple Mail folders, or directories of them. */
  inputs: string[];
  /** Where the decoded messages are staged (default: the system's temporary directory). */
  tempDir?: string;
}

/** Why a conversion failed. `kind` is pstsmith's error code, or one of this package's own: `binary-not-found`,
 * `aborted`, `timeout`, `protocol-invalid`, `failed`. */
export class PstsmithError extends Error {
  readonly kind: string;
  readonly exitCode: number | null;
  readonly itemKey: string | null;
  readonly detail: string;

  constructor(kind: string, detail: string, exitCode: number | null = null, itemKey: string | null = null) {
    super(`${kind}: ${detail}`);
    this.name = "PstsmithError";
    this.kind = kind;
    this.detail = detail;
    this.exitCode = exitCode;
    this.itemKey = itemKey;
  }
}

const OUTPUT_CAP = 1 << 20;
const STDERR_CAP = 4 << 20;

function outputArgs(output: Output): string[] {
  if ("file" in output) return ["--output", output.file];
  return ["--output-dir", output.dir, ...(output.maxFileSize ? ["--max-file-size", output.maxFileSize] : [])];
}

function commonArgs(options: CommonOptions): string[] {
  return [
    ...(options.excluded ? ["--excluded", options.excluded] : []),
    ...(options.batch !== undefined ? ["--batch", String(options.batch)] : []),
    ...(options.batchBytes !== undefined ? ["--batch-bytes", String(options.batchBytes)] : []),
  ];
}

/** Converts a JSONL manifest of messages. */
export function convertManifest(options: ManifestOptions): Promise<Report> {
  return run(["--manifest", options.manifest, ...outputArgs(options.output), ...commonArgs(options)], options);
}

/** Converts EML, MBOX and Maildir input. */
export async function convertMail(options: MailOptions): Promise<Report> {
  if (options.inputs.length === 0) throw new PstsmithError("usage", "at least one input is needed");
  // A private staging directory of this call, removed afterwards, also when the process had to be killed.
  const staging = mkdtempSync(join(options.tempDir ?? tmpdir(), "pstsmith-node-"));
  try {
    const output = "file" in options.output ? ["-o", options.output.file] : outputArgs(options.output);
    return await run(["convert", ...options.inputs, ...output, ...commonArgs(options), "--temp-dir", staging, "--json"], options);
  } finally {
    rmSync(staging, { recursive: true, force: true });
  }
}

function run(args: string[], options: CommonOptions): Promise<Report> {
  const binary = resolveBinary(options.binary);
  if (!binary || !existsSync(binary)) {
    return Promise.reject(new PstsmithError("binary-not-found", binary ? `${binary} does not exist` : "no pstsmith binary for this platform; set PSTSMITH_BINARY"));
  }
  if (options.signal?.aborted) return Promise.reject(new PstsmithError("aborted", "aborted before it started"));
  const outputPath = "file" in options.output ? options.output.file : options.output.dir;
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, { shell: false, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let stdoutBytes = 0;
    let stderrBytes = 0;
    let stopped: string | undefined;
    const stop = (kind: string) => {
      if (stopped || child.exitCode !== null) return;
      stopped = kind;
      child.kill("SIGKILL");
    };
    const onAbort = () => stop("aborted");
    options.signal?.addEventListener("abort", onAbort, { once: true });
    const timer = options.timeoutMs !== undefined ? setTimeout(() => stop("timeout"), options.timeoutMs) : undefined;
    child.stdout.on("data", (chunk: Buffer) => {
      stdoutBytes += chunk.length;
      if (stdoutBytes > OUTPUT_CAP) stop("protocol-invalid");
      else stdout.push(chunk);
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderrBytes += chunk.length;
      if (stderrBytes <= STDERR_CAP) stderr.push(chunk);
    });
    child.on("error", (error: NodeJS.ErrnoException) => {
      stopped ??= "spawn";
      cleanup();
      reject(new PstsmithError(error.code === "ENOENT" ? "binary-not-found" : "failed", error.message));
    });
    function cleanup() {
      if (timer) clearTimeout(timer);
      options.signal?.removeEventListener("abort", onAbort);
    }
    // A stopped conversion ends when its process exits: what it may have started does not hold the result back.
    child.on("exit", () => {
      if (!stopped || stopped === "spawn") return;
      cleanup();
      child.stdout.destroy();
      child.stderr.destroy();
      // A killed conversion could not remove what it wrote: the output (it did not exist before, pstsmith refuses an
      // existing one) and the exclusion record go.
      for (const path of [outputPath, options.excluded]) if (path) rmSync(path, { recursive: true, force: true });
      const detail = stopped === "protocol-invalid" ? "too much output on stdout" : stopped === "timeout" ? `stopped after ${options.timeoutMs} ms` : "stopped by the signal";
      reject(new PstsmithError(stopped, detail));
    });
    child.on("close", (code) => {
      cleanup();
      if (stopped) return;
      const errText = Buffer.concat(stderr).toString("utf8");
      if (code === 0) {
        try {
          resolve(parseResult(Buffer.concat(stdout).toString("utf8")));
        } catch (error) {
          reject(new PstsmithError("protocol-invalid", error instanceof ProtocolError ? error.message : String(error)));
        }
        return;
      }
      const line = parseError(errText);
      if (line) reject(new PstsmithError(line.error, line.detail, code, line.item_key));
      else reject(new PstsmithError("failed", errText.split("\n")[0] || `exit code ${code}`, code));
    });
  });
}
