// The pst-export-result/1 line pstsmith prints on stdout, and its error line on stderr, read strictly: a field this
// version does not know is refused rather than ignored, so a change of the format cannot pass unnoticed.

/** The program that wrote the file. */
export interface Engine {
  name: string;
  version: string;
  writer?: string;
}

/** One PST file written. */
export interface OutputFile {
  path: string;
  bytes: number;
}

/** The report of a finished conversion (`pst-export-result/1`; docs/MANIFEST.md lists every field). */
export interface Report {
  protocol: "pst-export-result/1";
  engine: Engine;
  manifest: { schema: string; export_id: string; item_count: number; sha256: string };
  /** Items read. */
  read: number;
  /** Items written. */
  written: number;
  /** Items left out by name (recorded in the exclusion record). */
  skipped: number;
  /** Warnings of a mail conversion; their text is on stderr. */
  warnings: number;
  batches: number;
  /** The file written, or null when the conversion wrote parts into a directory. */
  output: OutputFile | null;
  /** The parts written into a directory, in order. */
  outputs?: OutputFile[];
  /** What could not be written faithfully, by reason, with counts. */
  unsupported: Record<string, number>;
  excluded?: { path: string; items: number; sha256: string };
  file_size_limit?: { bytes: number; files_above: Array<{ path: string; bytes: number; item_key: string | null }> };
}

/** The error line: `{"error", "item_key", "detail"}`. */
export interface ErrorLine {
  error: string;
  item_key: string | null;
  detail: string;
}

const RESULT_FIELDS = new Set(["protocol", "engine", "manifest", "read", "written", "skipped", "warnings", "batches", "output", "outputs", "unsupported", "excluded", "file_size_limit"]);

export class ProtocolError extends Error {}

function object(value: unknown, what: string, fields?: Set<string>): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new ProtocolError(`${what} must be an object`);
  const record = value as Record<string, unknown>;
  if (fields) {
    for (const key of Object.keys(record)) if (!fields.has(key)) throw new ProtocolError(`unknown field ${what}.${key}`);
  }
  return record;
}

function count(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new ProtocolError(`${what} must be a non-negative integer`);
  return value;
}

function text(value: unknown, what: string): string {
  if (typeof value !== "string") throw new ProtocolError(`${what} must be a string`);
  return value;
}

function file(value: unknown, what: string): OutputFile {
  const record = object(value, what, new Set(["path", "bytes"]));
  return { path: text(record.path, `${what}.path`), bytes: count(record.bytes, `${what}.bytes`) };
}

/** Reads stdout: exactly one JSON line. */
export function parseResult(stdout: string): Report {
  const lines = stdout.split("\n").filter((line) => line.trim() !== "");
  if (lines.length !== 1) throw new ProtocolError(`expected one result line, got ${lines.length}`);
  let parsed: unknown;
  try {
    parsed = JSON.parse(lines[0]);
  } catch {
    throw new ProtocolError("the result line is not JSON");
  }
  const top = object(parsed, "result", RESULT_FIELDS);
  if (top.protocol !== "pst-export-result/1") throw new ProtocolError(`unsupported protocol ${String(top.protocol)}`);
  const engine = object(top.engine, "engine", new Set(["name", "version", "writer"]));
  const manifest = object(top.manifest, "manifest", new Set(["schema", "export_id", "item_count", "sha256"]));
  const unsupported = object(top.unsupported, "unsupported");
  const report: Report = {
    protocol: "pst-export-result/1",
    engine: { name: text(engine.name, "engine.name"), version: text(engine.version, "engine.version"), ...(engine.writer === undefined ? {} : { writer: text(engine.writer, "engine.writer") }) },
    manifest: { schema: text(manifest.schema, "manifest.schema"), export_id: text(manifest.export_id, "manifest.export_id"), item_count: count(manifest.item_count, "manifest.item_count"), sha256: text(manifest.sha256, "manifest.sha256") },
    read: count(top.read, "read"),
    written: count(top.written, "written"),
    skipped: count(top.skipped, "skipped"),
    warnings: top.warnings === undefined ? 0 : count(top.warnings, "warnings"),
    batches: count(top.batches, "batches"),
    output: top.output === null || top.output === undefined ? null : file(top.output, "output"),
    unsupported: Object.fromEntries(Object.entries(unsupported).map(([key, value]) => [key, count(value, `unsupported.${key}`)])),
  };
  if (top.outputs !== undefined) {
    if (!Array.isArray(top.outputs)) throw new ProtocolError("outputs must be a list");
    report.outputs = top.outputs.map((value, index) => file(value, `outputs[${index}]`));
  }
  if (top.excluded !== undefined) {
    const excluded = object(top.excluded, "excluded", new Set(["path", "items", "sha256"]));
    report.excluded = { path: text(excluded.path, "excluded.path"), items: count(excluded.items, "excluded.items"), sha256: text(excluded.sha256, "excluded.sha256") };
  }
  if (top.file_size_limit !== undefined) {
    const limit = object(top.file_size_limit, "file_size_limit", new Set(["bytes", "files_above"]));
    if (!Array.isArray(limit.files_above)) throw new ProtocolError("file_size_limit.files_above must be a list");
    report.file_size_limit = {
      bytes: count(limit.bytes, "file_size_limit.bytes"),
      files_above: limit.files_above.map((value, index) => {
        const above = object(value, `file_size_limit.files_above[${index}]`, new Set(["path", "bytes", "item_key"]));
        return { path: text(above.path, "path"), bytes: count(above.bytes, "bytes"), item_key: above.item_key === null ? null : text(above.item_key, "item_key") };
      }),
    };
  }
  return report;
}

/** Reads stderr's first line as the error line, if it is one. */
export function parseError(stderr: string): ErrorLine | undefined {
  const first = stderr.split("\n").find((line) => line.trim() !== "");
  if (first === undefined) return undefined;
  try {
    const record = object(JSON.parse(first), "error line");
    if (typeof record.error !== "string" || typeof record.detail !== "string") return undefined;
    return { error: record.error, item_key: typeof record.item_key === "string" ? record.item_key : null, detail: record.detail };
  } catch {
    return undefined;
  }
}
