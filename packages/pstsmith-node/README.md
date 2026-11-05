# pstsmith for Node.js

Convert EML files, MBOX files (Google Takeout, Thunderbird, Apple Mail) and Maildir directories into Outlook PST files,
from Node.js, on Linux, macOS and Windows, without Outlook.

```sh
npm install pstsmith
```

```ts
import { convertMail, PstsmithError } from "pstsmith";

try {
  const report = await convertMail({
    inputs: ["Takeout/Mail/All mail Including Spam and Trash.mbox"],
    output: { dir: "parts", maxFileSize: "10GB" },
    signal: AbortSignal.timeout(60 * 60 * 1000),
  });
  console.log(`${report.written} messages in ${report.outputs?.length} files`);
} catch (error) {
  if (error instanceof PstsmithError) console.error(error.kind, error.detail);
}
```

`convertManifest({ manifest, output })` converts a JSONL manifest of messages
([format](https://github.com/baozdemir/pstsmith/blob/main/docs/MANIFEST.md)).

The conversion runs the prebuilt `pstsmith` binary of your platform (installed as an optional dependency,
`@pstsmith/<platform>`) as a child process, without a shell: a crash or a memory blow-up never takes down your process,
and `signal` or `timeoutMs` kill it and remove what it wrote. The result is pstsmith's `pst-export-result/1` report,
read strictly: an unknown field is an error, not ignored. Set `binary` or `PSTSMITH_BINARY` to use another build.

See the [repository](https://github.com/baozdemir/pstsmith) for how the output is verified.
