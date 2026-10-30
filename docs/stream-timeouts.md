# Streaming deadlines and incomplete responses

Loop uses the same native streaming policy for every provider and model. There
are no model-name exceptions. Set millisecond overrides in agent settings:

```json
{
  "timeouts": {
    "connectionMs": 10000,
    "responseHeaderMs": 120000,
    "transportIdleMs": 900000,
    "modelProgressIdleMs": 900000,
    "generationBudgetMs": 0,
    "runBudgetMs": 0
  },
  "timeoutOverrides": {
    "providers": {
      "example-provider": { "responseHeaderMs": 180000 }
    },
    "models": {
      "example-provider/example-model": { "generationBudgetMs": 1800000 }
    }
  }
}
```

These are the defaults. Missing fields inherit; zero disables that particular
limit. Trusted project settings overlay explicitly supplied fields. Provider
settings override the base settings, and exact `provider/model` settings override
provider settings. Provider/model overrides are resolved again for each generation;
the run budget is fixed when the agent loop starts. Tools retain their own limits.
External ACP agents retain their own timeout policies.

Connection establishment retains its short 10-second bound. Response headers get
120 seconds for slow admission. The two 15-minute body idle windows accommodate
long reasoning and pauses while imposing finite stall protection. Total generation
and run budgets are opt-in so an actively progressing long request is not stopped
just because it has been running for several minutes.

The legacy `responseHeaderTimeoutMs` remains supported, including explicitly
configured 60000 values. `timeouts.responseHeaderMs` takes precedence. `/ttfb`
changes the base header policy; a provider/model override remains more specific.
`/ttfb on` chooses 120 seconds. Library `StreamOptions.timeout_ms` is a legacy alias
for `generationBudgetMs`.

## What resets a timer

Incoming body bytes reset transport activity, before parsing SSE frames. Comments
and fragmented frames therefore count as transport activity. Parsed nonempty
reasoning, answer text, and tool argument deltas establish model progress. Pings,
usage-only events, empty deltas and malformed events do not establish progress.
Heartbeats alone cannot extend the model-progress window. Time before the first
body bytes is covered by both idle windows after headers arrive.

The generation budget includes sending the request, connection/header waits,
network retries, protocol fallback requests, and reading the body. It never resets
on progress. The run budget covers the agent loop across generations and tools;
it excludes CLI startup and session loading. Short model-list/auth requests keep
their separate 30-second HTTP timeout. Pool idle expiry is not a response deadline.

## Failures, cancellation and retries

Chat Completions requires a finish reason or `[DONE]`; bare EOF is incomplete.
Final tool arguments must be valid JSON. Error and cancellation events retain the
partial response. The agent never executes tools from an incomplete/error response.
Completed messages and tool results remain available; failed assistant attempts
remain excluded from subsequent provider requests. Explicit user cancellation
retains the existing aborted-turn persistence policy.

Before response headers, network failures can retry twice with 1- and 2-second
backoff. There is no automatic regeneration after partial output, and no stream
resume claim. HTTP error statuses are reported rather than automatically replayed.
Protocol-specific reasoning-parameter fallbacks remain bounded to one retry.
Error-body reading is capped at 64 KiB and observes cancellation and deadlines.
Timed-out workflow agent tasks are not automatically restarted because tools may
already have executed. Dropping a native stream stops its producer.

On stream expiry, structured diagnostics include deadline type, threshold, elapsed
time, last-byte and last-progress ages, byte/chunk/event counts, and network attempt.
They omit credentials and raw reasoning. Header and run errors identify their
separate limits. An externally supplied HTTP client can impose additional limits;
configuring a connection override uses a standard Loop streaming client.

## Evidence and validation

The previous Chat Completions watchdog timed `eventsource.next()` at 300 seconds,
not individual reads. Comments could look silent and non-progress data events could
keep it alive. Reasoning deltas were already emitted; visible text was not required.
The streaming reqwest client already had no total request timeout.

A local adapter probe reproduced successful EOF after reasoning, `ToolUse` after
incomplete tool JSON, and loss of partial content on the 300-second timeout. The
regressions now reject incomplete EOF/tool arguments and retain partial data.
Virtual-time tests cover active reasoning beyond the old deadline, complete silence,
heartbeat-only activity, total generation expiry, and run-budget expiry. Existing
provider conformance tests cover cancellation, tool streaming and reasoning history.

A rebuilt CLI smoke check against the configured GLM-5.3 endpoint completed with
exit 0 in 2.87 seconds and emitted 14 stream-json lines. This confirms basic live
compatibility, not a reproduction of the original long-running incident. No logs
were available to attribute that incident to a particular local or upstream limit.
A gateway can still buffer reasoning, enforce a shorter deadline, or close the
connection; Loop cannot recover unseen output or resume an unsupported stream.
