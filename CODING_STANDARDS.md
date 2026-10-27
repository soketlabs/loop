# Coding standards

These are judgement calls for reviewing new Rust changes. Apply them to the
diff under review; don't retrofit existing code. Tooling enforces the
mechanical rules: `cargo fmt`, clippy with `-D warnings`, and the
`[workspace.lints]` table in `Cargo.toml`. Don't repeat those here.

The full list of what was adopted and skipped from the Google Rust style guide
is in [docs/design/rust-style-adoption.md](docs/design/rust-style-adoption.md).

## API shape

- **Unclear `bool` parameters.** If a call site would read `foo(true)` and
  you can't tell what `true` means, take a two-variant `enum` instead.
  Setters such as `set_enabled(bool)` are fine.
- **Newtype over type alias** when values could be mixed up (IDs, paths,
  units) or the type needs its own methods. An alias is fine as a shorthand.
- **Named fields over tuples.** Tuples are fine for single-field newtypes and
  short-lived local values.
- **Errors.** Use `Option` for "not there", `Result` when the caller can
  recover and needs to know why, and `panic`/`expect` only for broken
  invariants.

## Readability

- **Explain why an assumption holds.** `expect` and `assert!` messages should
  say why the condition holds, not restate it:
  `expect("edges were validated above")`, not `expect("get failed")`.
- **Explicit types where inference hides them.** This applies to `.parse()`,
  `.into()`, `.collect()` and long iterator chains whose result type isn't
  obvious.
- **Short iterator chains.** If a chain needs a comment to follow, use a loop.
  Bind a chain to a named variable before a `for` loop instead of building it
  in the loop header.

## Concurrency

- **Sync locks.** Use `parking_lot::{Mutex, RwLock}` for synchronous locks.
  Don't recover a poisoned `std::sync` lock with `into_inner()`. Use
  `std::sync::Mutex` only where an API requires it (for example
  tracing-subscriber's `MakeWriter`), and handle poison there as an error.
