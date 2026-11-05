// Writes the platform packages @pstsmith/<key> from the release's binaries, for `npm publish` of each.
//   node platform-packages.mjs VERSION BINARIES_DIR OUT_DIR
// BINARIES_DIR holds one directory per Rust target with the pstsmith binary in it, as the release workflow lays out.
import { chmodSync, copyFileSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [version, binaries, out] = process.argv.slice(2);
const TARGETS = {
  "darwin-arm64": ["aarch64-apple-darwin", "darwin", "arm64", null],
  "darwin-x64": ["x86_64-apple-darwin", "darwin", "x64", null],
  "linux-arm64-gnu": ["aarch64-unknown-linux-gnu", "linux", "arm64", "glibc"],
  "linux-arm64-musl": ["aarch64-unknown-linux-musl", "linux", "arm64", "musl"],
  "linux-x64-gnu": ["x86_64-unknown-linux-gnu", "linux", "x64", "glibc"],
  "linux-x64-musl": ["x86_64-unknown-linux-musl", "linux", "x64", "musl"],
  "win32-x64": ["x86_64-pc-windows-msvc", "win32", "x64", null],
};
for (const [key, [target, os, cpu, libc]] of Object.entries(TARGETS)) {
  const dir = join(out, key);
  const exe = os === "win32" ? "pstsmith.exe" : "pstsmith";
  mkdirSync(join(dir, "bin"), { recursive: true });
  copyFileSync(join(binaries, target, exe), join(dir, "bin", exe));
  // The MIT notice goes with every copy of the binary.
  copyFileSync(new URL("./LICENSE", import.meta.url), join(dir, "LICENSE"));
  if (os !== "win32") chmodSync(join(dir, "bin", exe), 0o755);
  const manifest = {
    name: `@pstsmith/${key}`,
    version,
    description: `The pstsmith binary for ${os} ${cpu}${libc ? ` (${libc})` : ""}; installed by the pstsmith package.`,
    license: "MIT",
    repository: { type: "git", url: "git+https://github.com/baozdemir/pstsmith.git" },
    os: [os],
    cpu: [cpu],
    ...(libc ? { libc: [libc] } : {}),
    files: ["bin", "LICENSE"],
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
}
