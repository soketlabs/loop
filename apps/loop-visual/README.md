# Loop Visual

A macOS desktop companion that turns what you are looking at into an interactive Three.js explanation. Ask a question about the screen, rotate the resulting scene, change a parameter, and ask a follow-up in the same conversation.

## How it works

```text
fresh screenshot + your question + current scene/control state
  → Loop session using GPT-6.1 Sol through OpenRouter
  → plan_visual: illustration brief and learning objective
  → show_visual: generated interactive Three.js scene
  → isolated floating window with local animation, orbit and zoom
```

Sol designs the composition, camera, labels, causal animation and useful controls before generating the scene. A render is rejected unless the current request has a design brief. Three.js and OrbitControls are bundled locally; generated scenes do not load scripts from a CDN.

## Requirements

- macOS, one primary display, and a working GPU/WebGL context.
- Rust 1.88 or newer and the build prerequisites for this Loop checkout.
- Node.js 22.12 or newer and npm.
- An OpenRouter account with credit and GPT-6.1 Sol access, connected through Loop.
- Recommended: [standalone Cua Driver](https://cua.ai/docs/cua-driver/quickstart), with its `cua-driver` CLI in PATH. Electron capture is the fallback.

## Quick start

From the repository root, build and run the CLI from this checkout to configure your provider:

```sh
cargo run -p loop-cli --bin loop
```

In Loop, run `/login` and connect OpenRouter. Exit the CLI, then:

```sh
cd apps/loop-visual
npm ci
npm run dev
```

`npm run dev` builds the Rust sidecar, bundles Three.js, and opens Electron. Subsequent launches can use `npm start` until Rust code changes.

The default model is **openrouter/openai/gpt-6.1-sol**. It reuses credentials from `~/.loop/agent`; export `LOOP_CODING_AGENT_DIR` if your Loop configuration lives elsewhere. Keys remain in the Rust sidecar and are never sent to the generated visual.

To override the model for this app only:

```sh
LOOP_VISUAL_MODEL=openrouter/openai/gpt-6.1-sol npm run dev
```

Any override must support images and tools. Cheaper models may produce weaker illustrations or fail to use the visual tools reliably. API usage is billed to your provider account; background captures do not trigger inference. Each response is capped at 8192 output tokens, which is not a dollar spending limit.

## Grant screen access

Click **Permissions** to start `cua-driver permissions grant`. Approve CuaDriver's Accessibility and Screen Recording permissions in macOS Settings. If capture remains pending, quit and reopen CuaDriver, then retry. This restart was necessary on the development machine.

If using Electron fallback, grant Screen Recording to Electron (or the responsible terminal/app identified by macOS) and relaunch. The status line identifies the capture backend and reports failures.

## Use it

1. Keep the source diagram, code or article visible beside the floating panels.
2. Enter your question and click **Explain screen**. A fresh screenshot and your exact text are sent together.
3. Wait for the design step and generated visual. Drag to orbit, scroll to zoom, and use the generated controls.
4. Ask a follow-up such as “Double this parameter and show the effect.” The current scene and reported control values accompany the new screenshot.
5. Use **Stop** to cancel generation or the close-visual button to dismiss the scene. Closing the input window exits the app.

Example: “Explain electromagnetic induction using an animated coil and moving magnet. Add a slider for magnet speed and show how the induced current changes.”

## Boundaries and limitations

This is a hackathon prototype, verified on macOS. It supports one primary display, one ephemeral conversation and full-scene replacement. Capture runs every three seconds and on questions. Windows remain visible to avoid flicker, so they can obscure source content in the screenshot. Move them aside when needed.

Generated content runs in an opaque-origin sandbox without Node, filesystem access, network requests or privileged desktop IPC. A bounded JSON bridge carries selection/control state. The agent receives only design and visual-output tools, not shell, file-editing or desktop-control tools.

Screenshots are kept in memory and removed from stored conversation messages; the session is also memory-only. Questions, screen pixels and scene context are sent to the selected model provider. Render quality, scientific accuracy and model adherence vary; inspect the explanation and its assumptions.

## Development checks

```sh
cargo test -p loop-visual
cargo clippy -p loop-visual -- -D warnings
cd apps/loop-visual
npm run check
npm test
npm run build:visual
npm run test:renderer
```

The sidecar integration tests use an isolated localhost model fixture. The native renderer smoke test verifies WebGL, controls, frame isolation and blocked network access; it requires a graphical desktop. See [DEMO.md](DEMO.md) for the walkthrough and verification boundary.
