# Q1b: why Loop gives incorrect results in Kubernetes benchmark run A

Status: analysis of existing data only. No model tokens, no builds, no cluster runs.

## 1. Scope and data

- Run: `scored-20261105-200622-3c0ce1` (run A, 5 repetitions, 9 harnesses x 13 tasks x 4 memory limits, 2,340 trials, model GLM-5.3).
- Loop version under test: commit `1dc7ef4a` (the same commit as `staging` HEAD at the time of this analysis). All file:line references below are for this commit.
- Data that we used (benchmark repo, read only):
  - `report/run-a/results.csv` and `summary.json`.
  - `exports/run-a/scored-20261105-200622-3c0ce1.tar.gz`. For each trial, it has `attempt.json`, `result.json` (stdout tail, stderr tail, exit code, usage, submission-copy summary), `outcome.json` and `verify/verdict.json` (all grader checks with detail).
  - Task definitions: `tasks/core-coding/*.yaml`. Grader: `bench/verification.py`. Harness adapter: `bench/harnesses.py`. Trial worker: `bench/worker.py`.
- Data that is NOT available locally:
  - No transcripts, no Loop session JSONL, no diffs, no workspace snapshots. The export README says that the full per-trial artifacts (about 11 GB: workspaces, harness state with the Loop `sessions/` directory, logs) stay on the cluster PVC. We did not read the PVC.
  - Thus, for each trial we know only: the final stdout (the final assistant text), stderr tail, exit code, token and turn counts, the submission-copy size, and the grader checks. We do not know the tool-call sequence.
  - The submission-copy size is a usable proxy for "did the agent change the workspace". The pristine `dark-mode-site` copy is 65,115 bytes, 14 files (the same value appears on OOM-killed trials of other harnesses). The pristine `mini-calc-buggy` copy is 1,999 bytes, 4 files.
- Harnesses in run A: cline, deepseek-harness, goose, hermes, kilo, loop, omp, opencode, pi. Claude Code and Codex are NOT in run A. All harnesses used the same model and the same endpoint.

## 2. The facts in numbers

Outcomes that are not `passed` or `oom` (all 2,340 trials):

| harness | incorrect | timeout |
|---|---|---|
| loop | 12 | 1 |
| hermes | 1 | 1 |
| deepseek-harness | 1 | 0 |
| goose | 1 | 0 |
| opencode | 1 | 0 |
| others | 0 | 0 |

Loop's 13 non-passing trials (rep, memory MiB, attempt id prefix):

| # | task | rep / MiB | attempt | turns | out tokens | stdout | workspace changed? | grader failure |
|---|---|---|---|---|---|---|---|---|
| 1 | TOOL-01 | 1 / 1024 | f9207146 | 22 | 1,642 | "Done ... made all fixes in /tmp/project" | no (1,999 B = pristine) | cargo tests, CLI add, clippy |
| 2 | TOOL-01 | 2 / 1024 | f09b3b74 | 18 | 1,360 | "Done ... corrected file lives at /tmp/workcopy" | no (1,999 B) | cargo tests, CLI add, clippy |
| 3 | FEAT-04 | 5 / 256 | 9e9a61e5 | 28 | 22,124 | "edit tool and chmod-based writes were denied ... built in /tmp/aurora" | no (65,115 B) | website tests |
| 4 | FEAT-05 | 4 / 1024 | 35e44036 | 39 | 10,438 | "couldn't modify the repo directly ... writable copy at /work/tmp/formval" | no (65,115 B) | website tests |
| 5 | FEAT-04 | 2 / 2048 | 79fd38fe | 18 | 61,255 | EMPTY, exit 0 | no (65,115 B) | website tests |
| 6 | FEAT-04 | 4 / 2048 | f813599d | 19 | 46,867 | EMPTY, exit 0 | no (65,115 B) | website tests |
| 7 | FEAT-02 | 3 / 256 | a6b99fff | 6 | 23,496 | EMPTY, exit 0 | no (65,115 B) | website tests |
| 8 | REF-01 | 2 / 1024 | 4e6e153e | 20 | 4,559 | correct helper, uses `parse::<i32>()` | yes | only "parse helper used for both operands" (`helper parses=0`) |
| 9 | REF-01 | 2 / 256 | 0f3bb428 | 20 | 4,971 | correct helper, uses `parse::<i32>()` | yes | same |
| 10 | REF-01 | 4 / 1024 | 84121302 | 17 | 2,965 | correct helper, uses `parse::<i32>()` | yes | same |
| 11 | REF-01 | 4 / 512 | 4dedae89 | 12 | 1,222 | helper hard-codes `"a must be integer"` | yes | "CLI preserves b must be integer error" |
| 12 | FEAT-02 | 3 / 1024 | b1784262 | 43 | 12,162 | "16/16 dark-mode tests pass ... remove that symlink" | yes | `unsafe_submission`: `node_modules` symlink |
| 13 | FEAT-02 | 5 / 512 | 7addad94 | 0 (not recorded) | 0 | EMPTY, killed at 1,200 s | no (65,115 B) | timeout, no verdict |

Rows 1-12 are the 12 incorrect verdicts (REF-01 x4, FEAT-04 x3, FEAT-02 x2, TOOL-01 x2, FEAT-05 x1). Row 13 is the extra FEAT-02 timeout.

In all 13 trials: `timed_out=false` except row 13, `cancelled=false`, exit code 0 except row 13 (signal 9), no error line on stderr, no OOM. The benchmark does not enforce a turn or token ceiling (`limits_disclosure: not-enforced`), so no trial stopped on a benchmark limit.

## 3. A condition that applies to all harnesses: the workspace starts read-only

`bench/worker.py` copies the fixture into `/work/project` with the image's modes: files `0400`, directories `0500`, owned by the agent user (`bench-agent`). Commit `815969d` ("Revert writable agent workspace") removed `make_tree_writable` on purpose, so that all harnesses get the same conditions. The agent can fix this with `chmod u+w`, because it owns the files.

All harnesses saw this condition. The final text mentions read-only files in 242 of 260 Loop trials and in 138 to 247 of 260 trials for each other harness. Most agents (Loop too, in most trials) ran `chmod u+w` and edited in place. Example, pi on TOOL-01 rep 1 / 1024 (the same slot as row 1): "the repo files were read-only, so I first ran `chmod u+w` on the project files", 9 turns, passed.

## 4. Root causes, ranked by number of failures

### Cause A (proven, 4 trials): Loop did the work in a scratch copy and reported "Done"; the real workspace did not change

Trials: rows 1, 2, 3, 4 (TOOL-01 x2, FEAT-04 x1, FEAT-05 x1).

Evidence (proven from the export):
- The final text of each trial says that Loop could not write to `/work/project`, that it copied the project to a writable directory (`/tmp/project`, `/tmp/workcopy`, `/tmp/aurora`, `/work/tmp/formval`), and that it made and verified the change there.
- The submission copy is byte-identical in size to the pristine fixture (1,999 B or 65,115 B). The grader output agrees: TOOL-01 still has `a - b` (`add(2,3) = -1`) and the original clippy errors; FEAT-04 has no `#favorites-count`.
- Loop ran the tests. It verified the wrong tree. Thus "verify before finish" did occur, but the verification target was not the working directory.
- Row 1 and row 2 say "no sudo" and "directory not writable". The files are owned by the agent user, so `chmod u+w` works. This is a wrong conclusion by the model.
- No other harness has an incorrect verdict of this type (0 of about 2,080 trials). Loop's own record shows the model can recover: Loop passed 247 trials, most of them after `chmod u+w`.

Contributing factors in the Loop code (inference; transcripts are necessary to prove which one was active in each trial):
- A1. The bash safety block list matches by substring on the lowercased command (`crates/loop-agent/src/harness/tools/mod.rs:324-357`). The pattern `"chmod -r 777 /"` also matches `chmod -R 777 /work/project`, and `"rm -rf /"` also matches `rm -rf /tmp/x` or `rm -rf /work/project/node_modules`. The pattern `"chown -r"` blocks every recursive chown. When this occurs, the tool returns "Command blocked by safety policy", which the model can read as "chmod is not permitted". Row 3 says: "both my `edit` tool and `chmod`-based writes were denied". `chmod` by the owner cannot fail with EACCES, so a block-list hit is the most probable explanation for that sentence. Other harnesses (pi, omp, etc.) do not have this block list.
- A2. The default system prompt (`crates/loop-app-core/src/system_prompt.rs:96-99`) has only two guidelines ("Be concise...", "Use tools to inspect the codebase..."). It does not tell the model that the result must be in the working directory, and it does not tell it what to do on a permission error.
- A3. The `edit` and `write` tools return the raw OS error (`map_io`, `crates/loop-agent/src/harness/env/host.rs:39-47`; tool bodies `tools/mod.rs:103-230`). The error does not say that the file is owned by the current user and that `chmod u+w` will fix it.
- A4. Nothing at the end of a headless (`--print`) run compares the final claim with the working tree. A run that changed zero files in `cwd` ends as "Completed", exit 0.

### Cause B (strongly indicated, 3 trials): the run ended silently with an empty final message; most probably an output-token-limit stop with no tool call

Trials: rows 5, 6, 7 (FEAT-04 x2, FEAT-02 x1).

Evidence (proven from the export):
- Exit code 0, empty stdout, no error on stderr (only the trace id). In text mode, `--print` writes the text of the last assistant message. Thus the last assistant message had no text.
- The workspace did not change (65,115 B). The run stopped before any edit reached `/work/project`.
- These 3 trials have the 3 highest output-tokens-per-turn values of all 259 Loop trials with usage: 3,916, 3,403 and 2,467 tokens per turn. The next value is 1,053, and the median is 138. All 3 have at least 16,384 output tokens in total.
- Empty stdout is rare for Loop: 5 of 260 trials (these 3, the timeout, and 1 pass). For omp and others, empty stdout is normal output format behavior, not a failure signal.

The code path (proven in code):
- The adapter does not set `maxTokens` for Loop (`bench/harnesses.py`, `loop_models_json`). Loop then uses the default 16,384 output tokens for a custom OpenAI-compatible model (`crates/loop-ai/src/providers/openai_compatible.rs:46`) and sends it as `max_tokens` with each request.
- When a response stops on `finish_reason=length` and contains no tool call, the agent loop sets `has_more_tool_calls = false` and ends the run (`crates/loop-agent/src/agent_loop.rs:362-391`). The truncation guard at `agent_loop.rs:365` applies only when the truncated response has tool calls.
- `turn_outcome` maps `Length` to `StopReason::MaxTokens` with no error (`crates/loop-agent/src/protocol_events.rs:568`). `run_turn` then reports `TurnEnd::Completed` (`crates/loop-cli/src/print_mode.rs:507`), and print mode writes the (empty) text with exit 0 (`print_mode.rs:197`). Text mode does not print the stop reason, so the benchmark cannot see it.

What is still an inference: we do not have the session JSONL, so we cannot see `stop_reason: length` on the last message. A second possible path with the same result is a final message with only reasoning content and `finish_reason=stop`. Both paths have the same fix (do not end silently on a final message without text or tool calls).

Note: pi has the same loop design and the same default output limit, and it did not fail in this way. Why Loop's responses became so long in these 3 trials is not known from this data.

### Cause C (proven, 3 Loop trials, plus 1 hermes and 1 deepseek-harness): grader false negative in REF-01

Trials: rows 8, 9, 10.

Evidence (proven):
- All behavior checks pass (cargo tests, CLI add/mul, unknown-op, both error messages, "CLI behavior preserved"). Only "parse helper used for both operands" fails with `calls=2, outside parses=0, helper parses=0`.
- The grader (`bench/verification.py:255-263`) counts helper parses with `\.parse\s*\(`. This regex does not match the turbofish form `.parse::<i32>(`. The regex for parses outside the helper accepts the turbofish form, so the two checks are not consistent.
- In all 5 REF-01 trials whose final text shows `parse::<i32>` (3 Loop, 1 hermes, 1 deepseek-harness), the verdict is incorrect. In all REF-01 trials without it, the check passes. The 3 Loop helpers are correct and reproduce the `expect` message format exactly.

This is not a Loop defect. With a corrected regex (for example `\.parse\s*(?:::\s*<[^>]*>\s*)?\(`), Loop has 9 incorrect verdicts, and run A has 11 in total (5 of 16 incorrect verdicts in run A are grader false negatives). We did not change the benchmark repo; this is a recommendation for its owner.

### Cause D (proven, 1 trial): Loop left a helper artifact in the workspace

Trial: row 12 (FEAT-02, 1024 MiB). The feature was correct (Loop reports 16/16 dark-mode tests pass). To run the tests, Loop created a symlink `node_modules -> /tmp/dmtest/node_modules` in the project and did not remove it. Its final text says "remove that symlink if you vendor dependencies normally". The safe-copy step rejects symlinks, so the verdict is `unsafe_submission`. No other harness in run A has a rejected submission.

Contributing factors (inference): no end-of-task step to review `git status` or the diff and remove temporary files (system prompt, A2). Possibly A1: an `rm -rf /work/project/node_modules` would be blocked by the `"rm -rf /"` substring.

### Cause E (proven, 1 trial): model logic error that a full self-check would find

Trial: row 11 (REF-01, 512 MiB). The helper `parse_i32_arg(args, index)` uses `.expect("a must be integer")` for both operands. Thus `add 2 nope` prints "a must be integer". Loop verified only `add x y` (the `a` path). No other harness made this error (0 of the other REF-01 trials). The harness link is weak: Loop gives no guidance to check each changed behavior path or to read its own diff before it finishes.

### Cause F (not determined, 1 trial): FEAT-02 timeout

Trial: row 13. The worker killed the process tree at 1,200 s (signal 9). Usage is 0 because Loop saves the session at the end of a turn, so a killed run records nothing. The workspace did not change. No trace id was printed, as expected for a kill. Hermes also had one timeout on FEAT-02. Possible causes: a stalled model stream (the SSE idle timeout is 900 s by default, `crates/loop-ai/src/api/sse.rs:303`), or long tool calls (bash: 120 s default, 600 s maximum, `tools/mod.rs:388-395`). The data cannot separate these causes.

### Causes that the data rules out

- Context overflow and compaction. `should_compact` (`crates/loop-agent/src/harness/compaction/mod.rs:51`) has no caller, but no failing trial came near the 128,000-token window. The largest cumulative input in a failing trial is 542,267 tokens over 43 calls (mean about 12,600 tokens per call). No overflow error is on stderr. Compaction did not cause any run A failure. It is still a gap for longer tasks.
- Turn, token or time limits, except row 13. The benchmark does not enforce them, and Loop has no default turn cap in `--print`.
- Cancelled tools, tool errors that ended the run, OOM, launch failures: none in the 12 incorrect trials (exit 0, `cancelled=false`, `oom_kill_delta=0`).

## 5. Summary of causes

| rank | cause | trials | proof level | Loop defect? |
|---|---|---|---|---|
| 1 | A: work left in a scratch copy; real workspace unchanged | 4 | outcome proven; trigger (block list vs. model judgment) is inference | yes |
| 2 | B: silent end with empty final message, workspace unchanged | 3 | outcome proven; `length` stop is a strong inference | yes |
| 3 | C: REF-01 grader regex rejects `parse::<i32>()` | 3 (+2 other harnesses) | proven | no (grader) |
| 4 | D: temporary symlink left in workspace | 1 | proven | partly (no finish hygiene) |
| 5 | E: logic error not caught by self-check | 1 | proven | weak |
| 6 | F: timeout, cause unknown | 1 (not in the 12) | not determined | unknown |

## 6. Proposed changes in Loop, ranked by expected effect

1. Make permission recovery work and keep work in `cwd` (cause A, 4 trials; also D).
   - `crates/loop-agent/src/harness/tools/mod.rs:324-357`: replace substring matching with a match on the parsed command and its target. Block `chmod -R 777 /` and `rm -rf /` only when the target is `/` (or `/*`, `~`), not any path that starts with `/`. Remove the blanket `"chown -r"` pattern or restrict it to `/`. Add unit tests: `chmod -R u+w /work/project`, `chmod -R 777 /work/project`, `rm -rf /tmp/x` must pass; `rm -rf /`, `chmod -R 777 /` must be blocked.
   - `crates/loop-app-core/src/system_prompt.rs:96-99`: add guidelines: "Make the requested changes in the working directory. Do not finish with changes only in a copy." and "If a write fails with permission denied, check the owner and mode (`ls -l`, `id`). If you own the file, restore write permission (`chmod u+w`) and continue."
   - `crates/loop-agent/src/harness/env/host.rs:39-47` and the `edit`/`write` tools (`tools/mod.rs:103-230`): on `PermissionDenied`, add a short hint to the error text (owner, mode, and that `chmod u+w` is permitted when the current user owns the file).
2. Do not end a run silently on a truncated or empty final message (cause B, 3 trials).
   - `crates/loop-agent/src/agent_loop.rs:362-391`: when `stop_reason == Length` and there are no tool calls (or the final message has neither text nor tool calls), append a short continuation message ("Your last response hit the output limit. Continue from where you stopped; use tools for large content.") and run another turn. Cap this at 2-3 retries.
   - `crates/loop-cli/src/print_mode.rs:504-508` and `:197`: when the turn ends with `MaxTokens` or with empty final text, write a line to stderr (`loop: turn ended on output limit`) and use a non-zero exit code, so a benchmark can see the cause.
   - `crates/loop-ai/src/providers/openai_compatible.rs:46`: review the 16,384 default output limit for custom models. (The benchmark adapter can also set `maxTokens` for Loop; that is a benchmark change, out of scope here.)
3. Add a finish step for headless runs (causes D and E, 2 trials; also a safety net for A).
   - System prompt (`system_prompt.rs:96-99`): "Before you finish, review `git status`/`git diff` of the working directory, remove temporary files and symlinks that you created there, and run tests for each behavior that you changed."
   - Optional, stronger: in `--print`, after the turn ends, warn on stderr when the working tree has no changes but the final text reports success. This needs a design decision, because some tasks need no edits.
4. Fix the REF-01 grader regex (cause C, 3 Loop trials). This is a change in the benchmark repo (`bench/verification.py:261`), not in Loop. It changes the reported number most for the least work.
5. Shorten the stream idle timeout and persist usage per turn (cause F, 1 trial, speculative).
   - `crates/loop-ai/src/api/sse.rs:303`: 900 s idle default is long compared with a 1,200 s task budget. Consider 120-300 s with a retry.
   - Save session state after each turn, so a killed run keeps its usage and transcript.
6. Not needed for run A: wiring `should_compact` into the loop. Keep it on the roadmap for long tasks, but it does not explain any run A failure.

Expected effect: changes 1 and 2 address 7 of Loop's 12 incorrect verdicts. Change 4 removes 3 more from the count. Change 3 targets the remaining 2.

## 7. Smoke set for Q1c (10 tasks)

Failing tasks (5): REF-01, FEAT-02, FEAT-04, FEAT-05, TOOL-01.

Passing tasks (5), chosen so that each fixture and each failure mechanism has a control:

| task | fixture | why |
|---|---|---|
| FEAT-03 | dark-mode-site | Same fixture as FEAT-02/04/05. Highest count of "worked in a copy" final texts among passing tasks (4 of 20), and its largest run used 16,783 output tokens, near the 16,384 per-response limit. A control for causes A and B. |
| FEAT-06 | dark-mode-site | Long runs (median 272 s, up to 27,733 output tokens). A control for cause B and for change 2 regressions. |
| AGENT-01 | mini-calc-buggy | Same fixture as TOOL-01. A multi-step "diagnose, fix, verify with shell" task; 2 of 20 final texts mention a copy. A control for cause A on Rust and for the bash block-list change. |
| FEAT-01 | mini-calc | Same fixture as REF-01. The grader checks both error messages ("a must be integer", "b must be integer"). A control for cause E and for the finish-step change. |
| WEB-REF-01 | mini-web | The only fixture not covered otherwise. It has a structural grader like REF-01 (goose's one incorrect verdict). A control for regressions on a third fixture. |

Not chosen: BUG-01, WEB-BUG-01, WEB-FEAT-01. They are short (7-10 turns, under 1,100 output tokens), with no copy or truncation signal, so they give little information.

Notes for Q1c:
- Use 1,024 MiB or more, so that OOM does not hide the result.
- The failure rates are 5-20% per task. One repetition per task cannot show a fix. Use at least 3 repetitions, or report the result as a smoke check only.
- Fix the REF-01 grader (change 4) before Q1c, or REF-01 will continue to fail for correct code.

## 8. Next step to close the open inferences

Read (read only) the Loop session JSONL from `harness_state` on the cluster PVC for the 13 attempts in section 2. Check:
- Rows 5, 6, 7: `stop_reason` and content of the last assistant message (cause B: `length` or not).
- Rows 1-4 and 12: search tool results for "Command blocked by safety policy" (cause A1) and for `chmod` calls and their results.
- Row 13: the time of the last recorded event (stream stall or long tool call).
