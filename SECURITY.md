# Security policy

pstsmith reads files that may come from anyone (PST, EML, MBOX), so a crash, hang, unbounded memory use or wrong
write on hostile input is a security issue, as is anything that lets one input change another's output.

## Reporting a vulnerability

Please report it privately through [GitHub's private vulnerability reporting](https://github.com/baozdemir/pstsmith/security/advisories/new),
not in a public issue. Include the version, the platform, and a synthetic input that shows the problem (never a real
mailbox). You will get an answer within 14 days, and a fix or a plan with a date after the problem is confirmed.
Reporters are credited in the advisory unless they ask not to be.

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | yes |

## What pstsmith does to stay safe

- No `unsafe` code in the conversion pipeline. The nesting of attached e-mails is capped at 32 levels, MIME trees are
  walked and dropped without recursion, and memory follows the largest single message (plus a short record per
  message), not the size of the input.
- The staging directory of a mail conversion is private to the user (0700 on Unix, files 0600) and removed when the
  conversion ends, whether it succeeds or fails; `pstsmith convert` also removes it, and its partial output, when it
  is stopped by Ctrl-C (on Unix also SIGTERM, SIGQUIT or SIGHUP). A second Ctrl-C, a kill that cannot be caught
  (SIGKILL, an out-of-memory kill) and, on Windows, Ctrl-Break, closing the console window or ending the process
  leave its `pstsmith-*` directory in the temporary directory, holding decoded mail, and any partial output behind;
  remove them. SIGQUIT ends it without a core dump. The command line's signal setup is its only `unsafe` code (one
  `sigaction` call that reads whether a signal is ignored).
- Every release binary carries a build provenance attestation (`gh attestation verify`).
