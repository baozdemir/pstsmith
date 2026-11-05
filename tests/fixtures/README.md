# Test fixtures

Every message here is synthetic: every address is in `example.invalid`, and no text comes from real mail. The files
are compared byte for byte, so `.gitattributes` keeps git from converting their line endings.

- `manifest/`: manifests of the JSONL input format (`docs/MANIFEST.md`), v1 and v2, with the attachment files they
  name in `manifest/files/`. Paths are written as `@FIXTURES@/files/...`; the test resolves them. The files checked
  with SCANPST, classic Outlook and a Microsoft 365 import (`docs/VERIFICATION.md`) were written from these manifests by
  the same writer; they differ from the outputs here only in the store's display name, which is the output file's name.
  - `files/smime/`: S/MIME content made once with OpenSSL for a self-signed certificate, "Synthetic SMIME Sender"
    (`synthetic-signer-cert.pem`); its private key was deleted after use.
  - `files/large-64MiB.bin` is not committed: `python3 tests/fixtures/generate.py` writes it (the first MiB of
    SHAKE-256 of its name, repeated 64 times; the manifests pin its SHA-256).
- `reports/`: delivery and disposition reports (RFC 3464, RFC 3798), written by hand.
- `mail/`: EML, MBOX (Google Takeout, Thunderbird, Apple Mail, CRLF) and Maildir input, written by
  `python3 tests/fixtures/generate.py --mail`.
- `expected.sha256`: the SHA-256 of every output of `crates/pstsmith/tests/fixtures.rs`. After a deliberate change of
  output, rewrite it with `PSTSMITH_BLESS=1 cargo test -p pstsmith --test fixtures` and review the diff.
