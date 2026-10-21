# AGENTS.md

Guidance for working in this repository. It applies to all kinds of change:
refactors, optimizations, and new features alike.

## Project

Tools for reading and editing FST waveform files (the format written by
simulators such as Verilator and Icarus Verilog). The repository is a Cargo
workspace with two crates:

- **`fst-io`** — a library that parses the FST container format. It exposes a
  streaming `SectionIterator` over a file's blocks and typed readers for each
  block kind (header, geometry, hierarchy, value-change), decompressing the
  LZ4/zlib/gzip/FastLZ payloads the format uses. It is the authoritative model
  of the format; the binary builds on top of it.
- **`fst-tool`** — a command-line binary (built on `fst-io` and `clap`) that
  inspects and converts FST files.

Edition 2024. The library's only dependencies are the decompressors it needs
(`fastlz-rs`, `flate2`, `lz4_flex`); the binary adds `clap`. Prefer the
standard library over adding a new dependency, and raise it for discussion
before pulling one in.

## Layout

```
Cargo.toml              workspace manifest
fst-io/
  src/lib.rs            the FST parser: block types, section iterator, readers
  src/tests.rs          library unit tests (#[cfg(test)] mod tests)
fst-tool/
  src/main.rs           the CLI: argument parsing (clap) + each subcommand
docs/                   design notes and task logs
```

Both crates keep their code in a single source file today. When adding code,
follow the existing grouping within that file rather than splitting into new
modules without reason.

## Building and using

```sh
cargo build                     # debug build of the whole workspace
cargo build --release           # optimized build
cargo run -p fst-tool -- --help # see the CLI
```

`fst-tool` subcommands:

- `dump <file>` — print the block structure of a file.
- `info <file> --hier [--flat] [--names]` — print the variable hierarchy.
- `vcd <file> [-o out.vcd] [--start-time N] [--end-time N] [-j N]` — convert to
  VCD, optionally only a slice of the time range.
- `splice <file> <start> <end> <out>` — copy a file keeping only the value-change
  blocks that overlap a time range.

Run a subcommand with `cargo run -p fst-tool -- <subcommand> ...`, or use the
built binary at `target/release/fst-tool`.

## Code style

- Format with `cargo fmt` and keep the tree `cargo clippy`-clean. Both are part
  of validating a change (see below). Do not suppress a lint with `#[allow(...)]`
  to silence a real problem — fix the cause, and if an allow is genuinely
  warranted, keep it narrow and comment why.
- Match the surrounding code: naming, error handling (`std::io::Result` with the
  crate's `invalid_data` helper for malformed input), and the existing module
  organization. Prefer the simplest change that fits the established patterns
  over introducing a new abstraction.
- Make illegal states hard to represent: use the typed block/value enums rather
  than passing raw bytes or magic numbers around.
- Keep functions focused. Extract a helper when it clarifies intent, not to hit
  a line count.
- Solve the problem asked for. Do not add speculative configurability, extra
  abstraction layers, or unrelated cleanup in the same change.

## Comments

- Avoid comments. Prefer self-explanatory code — clear names and structure —
  over a comment that describes what the code does.
- Prefer doc-comments (`///`) on public items, and write one only when the item's
  contract is not obvious from its signature. Document the format detail or
  invariant a reader needs, not the mechanics of the code.
- When a comment is necessary, explain *why* — a constraint, a non-obvious
  consequence, or a format quirk that the code cannot express on its own.
- A comment must describe the current state of the code as it is. Never write a
  comment that explains a change, compares to a previous version, or narrates
  how the code used to work ("now we…", "changed to…", "previously this…").
- State what the code does, not what it does not do. Avoid the "X rather than Y"
  form — mentioning the alternative Y adds nothing; just say X.
- Do not restate what the code already makes clear. A `const` in a `static`
  block needs no "built at compile time" note, and a `return Err(...)` on an
  obvious guard needs no comment explaining the error condition.
- Keep comments brief and to the point. Delete commented-out code rather than
  leaving it behind.

## Commits

Follow [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): <summary>

<body>
```

- **Types**: `feat`, `fix`, `refactor`, `perf`, `docs`, `test`, `chore`, `build`.
- **Scope**: the crate or area touched, e.g. `fst-io`, `fst-tool`.
- **Summary**: imperative mood, lower case, no trailing period, and kept short
  (under ~62 characters).
- **Body**: wrap at ~76 columns. Explain what changed and why; put the detail
  here, not in the summary.
- Make one logical change per commit, and keep unrelated changes in separate
  commits. Commit only when asked.
- Never push the branches or create pull requests, only if the user explicitly
  asks.
- Never commit to main, unless the user explicitly asks.

## Validating a change

Before considering a change done, from the repository root:

```sh
cargo build --release           # it compiles
cargo test --workspace          # unit tests pass
cargo fmt --all                 # formatted
cargo clippy --workspace --all-targets   # lint-clean
```

Add or update tests when fixing a bug or adding a feature: a bug fix gets a test
that would have caught it, a new feature gets tests for its behavior. Tests live
in the `#[cfg(test)]` modules (`fst-io/src/tests.rs`, and the `tests` module in
`fst-tool/src/main.rs`); name each test for the property it asserts.

When a change could affect the bytes a tool writes (anything on the `vcd` or
`splice` output path), verify the output stays identical for inputs it should
not change — compare the before/after output of the same file rather than
trusting that the tests cover it.
