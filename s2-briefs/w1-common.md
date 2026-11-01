# S2 wave 1: common rules (read first)

- Design: docs/design/runtime.md (approved; follow it, including its "Decisions" list). Plan: /Users/anandgupta/.claude/plans/plan-the-s2-work-reactive-flurry.md.
- You work ONLY in your own git worktree (path given in your brief), on its branch. Never touch the main checkout or other worktrees. Three other agents work in parallel on other chunks; stay inside your chunk's files.
- Test-first: write failing tests, then the code.
- Before each commit, all must pass in your worktree:
  - cargo fmt --all -- --check
  - cargo clippy --workspace --exclude loop-desktop --all-targets --locked -- -D warnings
  - cargo clippy --workspace --exclude loop-desktop --all-targets --locked --features loop-agent/telemetry,loop-app-core/telemetry,loop-cli/telemetry -- -D warnings
  - cargo test --workspace --exclude loop-desktop  (you may run only affected crates while iterating; run the full set before the final commit)
  - If you change public API of loop-app-core: cargo +nightly check -p loop-desktop
- Do not change automatic compaction. Linux and macOS first; anything unix-only behind cfg(unix) with a clear message elsewhere; tests must not break on Windows.
- Use glossary terms exactly: Runtime, Session, Event Log, Surface, Escalation, Capability, Policy, Lock, Subagent.
- Commit with clear conventional messages (feat:/test:/refactor:) on your branch. Do NOT push, do NOT open PRs, do NOT merge.
- When done, reply with: branch, commit list (git log --oneline staging..HEAD), what you tested, and any deviation from the design doc and why. Keep it short.
