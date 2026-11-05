// Where the pstsmith binary is: the caller's path, PSTSMITH_BINARY, or the platform package npm installed.
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

/** The platform packages, `@pstsmith/<key>`, one per prebuilt binary. */
export function platformKey(): string {
  const { platform, arch } = process;
  if (platform === "linux") {
    const report = process.report?.getReport() as { header?: { glibcVersionRuntime?: string } } | undefined;
    return `linux-${arch}-${report?.header?.glibcVersionRuntime ? "gnu" : "musl"}`;
  }
  return `${platform}-${arch}`;
}

export function resolveBinary(given?: string): string | undefined {
  if (given) return given;
  if (process.env.PSTSMITH_BINARY) return process.env.PSTSMITH_BINARY;
  const exe = process.platform === "win32" ? "pstsmith.exe" : "pstsmith";
  try {
    const manifest = createRequire(import.meta.url).resolve(`@pstsmith/${platformKey()}/package.json`);
    return join(dirname(manifest), "bin", exe);
  } catch {
    return undefined;
  }
}
