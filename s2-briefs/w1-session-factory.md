# S2.3: split bootstrap into GlobalResources / WorkspaceResources / SessionFactory (no behaviour change)
Worktree: /Users/anandgupta/Soket/Code/LOOP/loop-wt/session-factory (branch s2/session-factory). Read w1-common.md in this folder first.
Scope: crates/loop-app-core (you own the refactor of src/runtime.rs, bootstrap ~1292-1626).
- Per design §2: GlobalResources (credentials, Models + refresh, session store/repository; never reads stdin), WorkspaceResources per cwd (trust via a non-interactive rule: never-trusted folder => global settings only + a notice; settings, resources, hooks, system prompt, HostExecutionEnv, web tools, MCP, lazy sandbox), SessionFactory::open(...) -> {harness, native, tool_gate, approval_bridge} that always sets the workspace explicitly (fix the process-cwd fallbacks in tool_gate.rs ~198 and hooks_load.rs ~250 when a workspace is given).
- bootstrap keeps its exact current behaviour for the TUI/--print/desktop (interactive trust prompt stays in bootstrap, not in the tiers) and becomes these three calls.
- Add a test seam to inject Models so the faux provider can be used.
- Keep loop_app_core::Runtime's name; add a doc comment pointing at the future loop_runtime::Runtime.
Tests FIRST (characterization before refactor): same tools, system prompt, permission mode and session as before; two Sessions in one cwd share WorkspaceResources but have separate gates and grants; two cwds => two workspaces; untrusted folder rule.
Run cargo +nightly check -p loop-desktop. Another agent adds a small --resume lease check in runtime.rs; keep your diff structured so that merge is easy (do not reorder unrelated code).
