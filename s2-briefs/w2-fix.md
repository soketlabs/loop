# Fix the wave 2 review findings on YOUR branch
Findings: /Users/anandgupta/.claude/jobs/b3fa4ea3/tmp/w2-review-out.md. Rules: /Users/anandgupta/.claude/jobs/b3fa4ea3/tmp/w2-common.md (test-first, all checks, commit, no push). CARGO_TARGET_DIR is already set in this shell.
- s2/runtime-core agent: fix P2-1, P2-2 (retry renewal with backoff; only give up after the lease is actually stale/lost, and then close the Session cleanly with a logged reason), P2-3 (keep last_active/latest seq in memory; don't scan the log in list()), P3-2, P3-3, P3-4, P3-5 (add the missing tests). Skip P3-1 unless trivial.
- s2/serve-infra agent: fix P2-4 (logs dir 0700, log file 0600), P3-6 (fallback path must be unique per agent dir, e.g. /tmp/loop-<uid>/<short hash of agent dir>.sock). Skip P3-7 (S2.7b holds the lock); add a doc comment saying the caller must hold RuntimeInstanceLock.
Add new commits (no amend). Reply with git log --oneline staging..HEAD and check results.
