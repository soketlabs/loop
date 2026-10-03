# Security scan policy

Security checks run on every PR (including stack branches), on main, daily at
03:17 UTC, manually, and as a reusable workflow before releases. They require no
provider API keys and run on disposable GitHub-hosted runners.

- cargo-deny blocks RustSec vulnerabilities and dependency repositories outside
  the explicit source allowlist. Yanked and unmaintained dependencies warn.
- License checks report findings without blocking while the license policy is
  being decided. Their job is explicitly informational.
- CodeQL scans Rust with the security-extended suite; its SARIF gate fails on
  security severity >= 7.0. Uploads appear in GitHub code scanning for ordinary
  runs; exact-commit release scans use local reports without tag uploads.
- Gitleaks CLI scans PR commit ranges and the current tree; main, daily, and
  release runs scan history too. Output is redacted; no secret reports are uploaded.
- Zizmor blocks medium/high findings with high confidence. Lower-confidence
  findings remain available through a local unfiltered scan.

Use the Gitleaks CLI, not the separately licensed organization GitHub Action.
Known fake credentials in tests need specific fingerprints or narrowly scoped
rules. Never ignore an entire tests directory or baseline a confirmed secret.
Rotate/revoke leaked credentials even after removing them from the tree.

## Exceptions

No blanket baseline is allowed. Each advisory ignore, code-scanning suppression,
secret fingerprint, or workflow suppression must include a reason, accountable
maintainer, and an expiry date alongside its configuration. Fix actual defects;
use suppressions only for demonstrated false positives or reviewed temporary
mitigations. Revisit exceptions when dependency versions or scanner rules change.

## Local checks

Use cargo-deny 0.20.2, Gitleaks 8.30.1, and Zizmor 1.30.1:

```sh
cargo deny --locked --all-features check advisories sources
cargo deny --locked --all-features check licenses # informational
# Run history scans on trusted local repositories only; do not paste findings.
gitleaks git --redact .
gitleaks dir --redact .
zizmor --offline --min-severity medium --min-confidence high .github/workflows
```

CodeQL requires the GitHub workflow. Its successful analysis step alone is not a
findings gate; the subsequent SARIF check enforces the security threshold.
