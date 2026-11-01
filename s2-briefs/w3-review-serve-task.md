# Review S2 S2.5 server/client branch (read-only; do not edit, commit or switch branches)
Repo /Users/anandgupta/Soket/Code/LOOP/loop. Branches, one commit each on top of staging: s2/protocol-serve (3dcb9bd). Worktree: ../loop-wt/protocol-serve.
Use git diff staging..<branch> and read the code. Check each against docs/design/runtime.md (approved, incl. its Decisions list) and the brief /Users/anandgupta/.claude/jobs/b3fa4ea3/tmp/w3-protocol-serve.md.
Report only actionable findings: correctness bugs (races, deadlocks, lost or duplicated events, seq gaps, lease leaks, permits never released), design mismatches, required tests missing, Windows build breakage, unsafe socket/lock handling. Verify each finding against the code before reporting; no style nits.
Write findings as Markdown to /Users/anandgupta/.claude/jobs/b3fa4ea3/tmp/w3-review-serve.md (severity P0-P3, branch, file:line, issue, fix). Reply only with the path and counts by severity.
Focus: attach gap/duplicate correctness, the earlier >1h test hang (is the root cause really gone?), per-connection filtering, silent drop when the 65,536 pending-update buffer overflows, v1 compatibility. Finish within 12 minutes.
