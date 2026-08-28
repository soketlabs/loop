# loop-desktop

GPUI desktop application for the Loop coding agent.

## Build requirements

- **Rust nightly** (GPUI currently requires `std::hint::cold_path`)
- Linux system libraries: `libfontconfig`, `libxcb`, `libxkbcommon`, Wayland/X11 dev packages
- On Debian/Ubuntu: `libfontconfig1-dev libxcb1-dev libxkbcommon-dev libwayland-dev`

Environment variables:

```bash
export RUST_FONTCONFIG_DLOPEN=1   # when fontconfig.pc is unavailable
export RUSTUP_TOOLCHAIN=nightly
```

## Run

```bash
cargo run -p loop-desktop -- /path/to/project
```

## Architecture

- `loop-app-core` — shared bootstrap, config, harness wiring
- `DesktopController` — bridges `AgentHarness` events to UI snapshot state (frame-coalesced notify ~60fps)
- `app.rs` — GPUI layout: sessions | chat + composer | diff, terminal dock at bottom

Features:
- Unified **ChatComposer** (textarea + model/thinking cycle buttons + token stats in one rounded box)
- **VirtualList** for sessions and chat rows
- Markdown assistant messages, thinking blocks, tool cards, clickable file-change chips
- Diff panel with +/- line highlights, accept/reject (git restore fallback), external editor open
- Tool approval modal (Accept / Reject / Always allow)
- PTY terminal dock (toggle with Ctrl+`)

## CI / release

Release workflow builds `loop-desktop` for all five platforms alongside `loop-cli`. Requires Rust **nightly** (see `crates/loop-desktop/rust-toolchain.toml`).
