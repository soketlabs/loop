# Fix the s2/store-lease review findings
1. First: git rebase s2/session-factory (it gained fix commit 6c7dfe4). Resolve conflicts.
2. Findings are in /Users/anandgupta/.claude/jobs/b3fa4ea3/tmp/w1-review-lease.md. Decisions:
   - P1 listing: store.load() must NOT take a lease. Take the lease only when a writer is admitted (harness opens/switches to a Session for writing, i.e. the single SessionFactory call site). Summaries/search/picker never lease and still show leased Sessions. Add the regression test the review describes.
   - P1 Windows: S2 is unix-first and the Runtime is cfg(unix). On non-unix, standalone bootstrap does not acquire leases at all (today's behaviour preserved); keep the lease API compiled and tested with injected liveness so Windows CI still runs the unit tests.
   - P2 recycled PIDs: when the caller passes reclaim proof (holds runtime.lock, owner "runtime"), takeover ignores pid liveness and heartbeat age. Ordinary takeover keeps stale AND dead. Add the review's recycled-PID test plus refusal without proof.
3. Test-first, new commit(s), no amend. Rerun fmt, both clippy configs, workspace tests, cargo +nightly check -p loop-desktop. Reply with git log --oneline staging..HEAD and results.
