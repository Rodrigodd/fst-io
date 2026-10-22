# Clock-sampled VCD dump

A `vcd` mode that, given one signal designated as a clock, emits the other
signals sampled only at that clock's edges. This turns an event-driven dump
(every value change, with intra-cycle glitches and combinational noise) into a
synchronous one that shows what the design latches each cycle.

## User surface

Two new flags on the `vcd` subcommand:

- `--clock <GLOB>` — selects the clock signal, reusing the existing
  `SignalFilter` glob matcher. It must resolve to **exactly one** signal, and
  that signal must be a **1-bit scalar** (`VariableLength::Bits(1)`):
  - zero matches is an error;
  - more than one match is an error, listing the matched paths;
  - a real, variable-length, or multi-bit match is an error.
- `--clock-edge <rising|falling|both>` — which clock transition triggers a
  sample. `requires = "clock"`, default `rising`. A `clap::ValueEnum`.

Composition with the existing `--signal` filter:

- `--clock` decides **when** to sample; `--signal` decides **which** signals
  appear. They compose freely.
- The clock does **not** need to appear in `--signal` to drive sampling; it is
  resolved independently from the full hierarchy.
- The clock signal is **not emitted** unless the user also selects it via
  `--signal` (or runs without a `--signal` filter, in which case it is emitted
  like any other signal). It is only a trigger.

## Sampling semantics (decided)

- **Edge**: `rising` fires on a transition landing on `1`; `falling` on a
  transition landing on `0`; `both` on any defined-to-defined transition. The
  first observed clock value is not an edge (no prior value to transition from).
  A transition touching `x`/`z` is not a rising/falling edge, and for `both`
  only `0/1 -> 0/1` counts.
- **Sample value**: strictly *before* the edge. At edge time `t`, each signal is
  sampled at its last change with time `< t`. A same-time change on a sampled
  signal (coincident with the clock edge) is excluded — setup-style.
- **Timestamp**: the real simulation time of the edge, `#<t>`. The original time
  axis is preserved; gaps between cycles remain.
- **Per-edge output**: only signals whose value changed since the previous
  *emitted* edge are written (standard compact VCD).
- **Empty edges**: if no sampled signal changed between two consecutive edges,
  no `#<t>` is emitted at all — the edge is skipped entirely.
- **Time range**: `--start-time` / `--end-time` clip which edges are emitted,
  exactly as the normal path clips changes. State before `start` is folded into
  the baseline.

## Why a separate, serial path

The existing `write_vcd_value_changes` decodes blocks **in parallel** (`-j`
workers) and pre-renders each block's changes independently, because a block's
output depends only on that block plus the static baseline. Clock sampling
breaks that independence: deciding what to emit at an edge needs the *full
current state of every signal* at that moment, which is an accumulation across
all prior blocks. So clock mode takes a **serial, time-ordered pass** that holds
the running state. `-j` keeps accelerating the normal (non-clock) dump; clock
mode ignores it. This tradeoff was accepted deliberately.

## Implementation

### Dispatch (`fn vcd`)

- `clock: Option<String>` and `clock_edge: ClockEdge` thread in from the parsed
  command.
- When `clock` is `None`: unchanged — the parallel path runs.
- When `clock` is `Some`: after reading header + geometry and walking the
  hierarchy, resolve the clock glob to its canonical geometry index (below),
  then call the new serial emitter instead of `write_vcd_value_changes`.
- Header and `$var`/`$scope` declaration writing is shared and unchanged. The
  clock is excluded from the emitted `vars` table unless `--signal` keeps it.

### Clock resolution

Walk the hierarchy with the same scope-stack logic `write_vcd_declarations`
uses to build full dotted paths. For each non-alias `Var`, test its path against
the `--clock` glob. Collect matches with their canonical (0-based, non-aliased)
geometry index. Enforce exactly one match and `VariableLength::Bits(1)`. Return
the geometry index and canonical id of the clock.

### Serial clocked emitter

New `write_vcd_value_changes_clocked(out, input, lengths, vars, header, start,
end, clock_geometry_index, clock_edge)`:

1. **Baseline / running state.** Reuse the existing `baseline` /
   `baseline_real` maps (id -> current ASCII value / real bytes) and
   `update_baseline`. These already model "current value of every signal"; here
   they are mutated continuously as the pass advances, not just folded once.
   Fold every block before `start` (and the first in-range block's pre-`start`
   changes) into the baseline exactly as today, so the state carried into the
   first edge is complete.
2. **Last-emitted snapshot.** A second map holds each signal's value as of the
   previously emitted edge, to compute "changed since last edge".
3. **Serial walk.** For each value-change block in order, decode its head/tail
   layout, time table, and per-signal waves (reusing `read_head_layout`,
   `read_tail_layout`, `read_positions`, `read_time_table`, `read_waves`). Merge
   all signals' changes into one stream ordered by time, carrying `(time, id,
   Value)`.
4. **Edge detection + sampling.** Process the merged stream in time order,
   holding `prev_clock_level`:
   - For a change on a non-clock signal at time `t`, apply it to the running
     state (`update_baseline`). Because sampling is strictly-before, a change at
     exactly an edge time must be applied *after* that edge is emitted — so
     within a single time step, edges are evaluated against state built from
     strictly-earlier times.
   - For a change on the clock at time `t`, compute the transition
     `prev_clock_level -> new_level`. If it matches `clock_edge`, fire: emit the
     edge at `#t`, writing every signal whose running state differs from the
     last-emitted snapshot, then update the snapshot. If no signal differs, emit
     nothing (skip the empty edge). Update `prev_clock_level`.
   - Respect `[start, end]`: edges with `t < start` or `t > end` do not emit,
     but still advance state and `prev_clock_level`.
5. **Initial `$dumpvars` frame.** The first fired edge seeds `$dumpvars` with
   the strictly-before running state (reusing `write_baseline`), matching how
   the parallel path seeds initial state, then the dumpvars state machine
   closes.

### Shared rendering

Per-value VCD encoding (`render_change`, scalar/vector/real formatting,
`vcd_id`) is reused verbatim, so a value emitted by clock mode is byte-identical
to the same value emitted by the normal path. Only *when* and *which* differ.

## Tests (`fst-tool` `tests` module)

- `clock_rising_samples_before_edge` — a signal changing at the same time as the
  edge is not reflected; its prior value is.
- `clock_edge_rising_falling_both` — same input under the three edge modes,
  asserting the emitted set of time steps.
- `clock_emits_only_changed_signals` — a signal steady across two edges appears
  once, not again.
- `clock_skips_empty_edges` — two edges with no change between them produce no
  second `#t`.
- `clock_must_be_one_bit_scalar` — a multi-bit or real clock is rejected.
- `clock_glob_zero_and_multiple_match` — both error cases.
- `clock_respects_time_range` — `--start-time` / `--end-time` clip edges.
- `clock_composes_with_signal_filter` — `--signal` narrows which signals; the
  clock still drives timing and is excluded from output unless selected.
- `clock_initial_dumpvars_frame` — the first edge seeds `$dumpvars` with
  strictly-before state.

## Validation

`cargo build --release`, `cargo test --workspace`, `cargo fmt --all`,
`cargo clippy --workspace --all-targets`. A non-clock `vcd` run must stay
byte-identical before/after the shared refactor — compared on a real file.
