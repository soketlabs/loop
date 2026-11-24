# Loop Visual demo

## 60-second walkthrough

1. Put a chart or chart-like screen on the primary display. Start Loop Visual and ask: “Explain the trend on screen and make an interactive chart that highlights the selected threshold.”
2. Wait for the visual window, move its slider, and confirm the value updates locally.
3. Ask a follow-up such as: “Keep that setting and explain how the highlighted range changes.” The new request captures the current display again and carries the previous visual and its control state forward.
4. Switch the primary display to an unrelated topic, such as a network diagram, and ask Loop Visual to explain it as a labeled flow diagram. Confirm that the second visual reflects the new screen.

Use a real model for this walkthrough. The model must support image input and return a `show_visual` tool call. Do not substitute a fixture or mock response for either topic.

## Verification and handoff status

- Rust sidecar build: passed.
- Rust Clippy: passed.
- Rust session test: one test passed; it checks that image blocks are replaced before session storage.
- Node tests: twelve passed. The sidecar integration case uses an isolated config and a localhost OpenAI-compatible SSE mock; it validates request and event handling, not real inference or visual quality.
- Native Electron renderer isolation smoke: passed. It checks inline `srcdoc` scripts, the slider-to-state bridge, iframe isolation, and blocked network access.
- Live Cua capture and desktop → OpenRouter Gemini Flash Lite → generated interactive visual: passed. Next advanced the content locally.
- Follow-up emitted a replacement, but inspection exposed a frame-navigation handler bug. It is corrected; native replacement re-verification remains pending.
- Visuals now explicitly position and focus on the primary display. Capture leaves window visibility/opacity unchanged to prevent flicker.
- Real two-topic demo: not yet passed. Three synthetic-source provider probes emitted no visual tool event; they do not establish that requirement.
- Cheap-model limitation: Gemini Flash Lite sometimes responds without show_visual. Panels may occlude captured content; keep source material beside them.

Three.js upgrade: native smoke passed with a real WebGL renderer and sphere inside the opaque sandbox, local control bridge and blocked network. Real-model 3D concept quality has not yet been verified. Try: “Explain electromagnetic induction with an animated Three.js coil and magnet. Add a slider for magnet speed.”

## Final Sol workflow

The app now defaults to GPT-6.1 Sol and requires plan_visual before show_visual. The user confirmed that this version works. This confirmation is not a controlled two-topic benchmark; the earlier two-topic validation remains incomplete. Rust and Node integration checks cover the design-before-render flow.
