//! Benchmark of the FST writer on a synthetic design shaped like a gate-level
//! simulation: a clock, many 1-bit nets, some buses, a few wide, four-state,
//! real and string signals.
//!
//! The changes of `PATTERN_STEPS` time steps are generated up front and
//! replayed, so the timed loop measures the writer and not the generator.
//!
//! ```sh
//! cargo run --release -p fst-io --example write_bench -- [--threads N]
//!     [--steps N (default 1000000)] [--block-size BYTES] [--dedup]
//!     [--output PATH]
//! ```
//!
//! Without `--output` the file is written to a sink that only counts bytes.

use std::io::{Seek, SeekFrom, Write};
use std::time::Instant;

use fst_io::{
    EncodeOptions, FstHierarchyWriter, FstSignalHandle, FstValueWriter, FstWriterOptions,
    ScopeType, VarDirection, VarType,
};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

const BITS: usize = 30_000;
const BYTES: usize = 3_000;
const WORDS: usize = 1_000;
const WIDE: usize = 200;
const FOUR_STATE: usize = 100;
const REALS: usize = 20;
const STRINGS: usize = 5;
const PATTERN_STEPS: usize = 4096;
/// Average changes per time step, besides the clock.
const CHANGES_PER_STEP: usize = 300;

/// A sink that keeps only the length of what is written.
#[derive(Default)]
struct Discard {
    position: u64,
    len: u64,
}

impl Write for Discard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.position += buf.len() as u64;
        self.len = self.len.max(self.position);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Discard {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.position = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => self.len.saturating_add_signed(offset),
            SeekFrom::Current(offset) => self.position.saturating_add_signed(offset),
        };
        Ok(self.position)
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Bit,
    U64,
    Wide,
    FourState,
    Real,
    String,
}

#[derive(Clone, Copy)]
struct Change {
    signal: u32,
    value: u64,
    high: u64,
}

struct Design {
    kinds: Vec<Kind>,
    handles: Vec<FstSignalHandle>,
    clock: FstSignalHandle,
}

struct Args {
    threads: usize,
    steps: usize,
    block_size: usize,
    deduplicate: bool,
    output: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        threads: 0,
        steps: 1_000_000,
        block_size: FstWriterOptions::default().block_size,
        deduplicate: false,
        output: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().expect("missing option value");
        match arg.as_str() {
            "--threads" => args.threads = value().parse().expect("--threads takes a number"),
            "--steps" => args.steps = value().parse().expect("--steps takes a number"),
            "--block-size" => {
                args.block_size = value().parse().expect("--block-size takes a number")
            }
            "--dedup" => args.deduplicate = true,
            "--output" => args.output = Some(value()),
            _ => panic!("unknown option {arg}"),
        }
    }
    args
}

fn declare<W: Write + Seek + Send + 'static>(
    writer: &mut FstHierarchyWriter<W>,
) -> std::io::Result<Design> {
    let mut kinds = Vec::new();
    let mut handles = Vec::new();
    writer.scope(ScopeType::Module, b"top", b"top")?;
    let clock = writer.var(VarType::Wire, VarDirection::Input, b"clk", 1)?;
    let groups: [(Kind, usize, VarType, u32); 7] = [
        (Kind::Bit, BITS, VarType::Wire, 1),
        (Kind::U64, BYTES, VarType::Reg, 8),
        (Kind::U64, WORDS, VarType::Logic, 32),
        (Kind::Wide, WIDE, VarType::Logic, 128),
        (Kind::FourState, FOUR_STATE, VarType::Logic, 16),
        (Kind::Real, REALS, VarType::Real, 64),
        (Kind::String, STRINGS, VarType::String, 0),
    ];
    for (group, &(kind, count, var_type, length)) in groups.iter().enumerate() {
        writer.scope(ScopeType::Module, format!("g{group}").as_bytes(), b"cell")?;
        for i in 0..count {
            let name = format!("n{i}");
            let handle = writer.var(var_type, VarDirection::Implicit, name.as_bytes(), length)?;
            kinds.push(kind);
            handles.push(handle);
            // Hierarchical designs see the same net from several scopes.
            if i % 16 == 0 {
                let alias = format!("a{i}");
                writer.alias(
                    var_type,
                    VarDirection::Implicit,
                    alias.as_bytes(),
                    length,
                    handle,
                )?;
            }
        }
        writer.upscope()?;
    }
    writer.upscope()?;
    Ok(Design {
        kinds,
        handles,
        clock,
    })
}

/// The changes of each pattern step. 1-bit nets make most of the changes, as
/// in a gate-level simulation.
fn generate(design: &Design) -> Vec<Vec<Change>> {
    let mut rng = StdRng::seed_from_u64(1);
    let bits = BITS as u32;
    let total = design.kinds.len() as u32;
    (0..PATTERN_STEPS)
        .map(|_| {
            let count = rng.random_range(CHANGES_PER_STEP / 2..CHANGES_PER_STEP * 3 / 2);
            let mut changes: Vec<Change> = (0..count)
                .map(|_| {
                    let signal = if rng.random_bool(0.8) {
                        rng.random_range(0..bits)
                    } else {
                        rng.random_range(bits..total)
                    };
                    Change {
                        signal,
                        value: rng.random(),
                        high: rng.random(),
                    }
                })
                .collect();
            changes.sort_by_key(|change| change.signal);
            changes.dedup_by_key(|change| change.signal);
            changes
        })
        .collect()
}

fn replay<W: Write + Seek + Send + 'static>(
    values: &mut FstValueWriter<W>,
    design: &Design,
    pattern: &[Vec<Change>],
    steps: usize,
) -> std::io::Result<u64> {
    const STRINGS: [&[u8]; 4] = [b"IDLE", b"FETCH", b"DECODE", b"EXECUTE"];
    let mut changes = 0u64;
    for step in 0..steps {
        values.time(step as u64 * 5)?;
        values.change_bit(design.clock, step % 2 == 0)?;
        let cycle = (step / PATTERN_STEPS) as u64;
        for change in &pattern[step % PATTERN_STEPS] {
            let signal = change.signal as usize;
            let handle = design.handles[signal];
            let value = change.value ^ cycle;
            match design.kinds[signal] {
                Kind::Bit => values.change_bit(handle, value & 1 == 1)?,
                Kind::U64 => values.change_u64(handle, value)?,
                Kind::Wide => {
                    let words = [
                        value as u32,
                        (value >> 32) as u32,
                        change.high as u32,
                        (change.high >> 32) as u32,
                    ];
                    values.change_words32(handle, &words)?;
                }
                Kind::FourState => {
                    // About one value in eight has x or z bits.
                    let bval = if value.is_multiple_of(8) {
                        change.high as u32
                    } else {
                        0
                    };
                    values.change_four_state(handle, &[value as u32], &[bval])?;
                }
                Kind::Real => values.change_f64(handle, (value % 10_000) as f64 / 7.0)?,
                Kind::String => values.change_var_len(handle, STRINGS[value as usize % 4])?,
            }
            changes += 1;
        }
    }
    Ok(changes + steps as u64)
}

fn run<W: Write + Seek + Send + 'static>(out: W, args: &Args) -> std::io::Result<u64> {
    let options = FstWriterOptions {
        threads: args.threads,
        max_blocks_in_flight: args.threads.max(1) + 1,
        block_size: args.block_size,
        encode: EncodeOptions {
            deduplicate: args.deduplicate,
            ..EncodeOptions::default()
        },
        ..FstWriterOptions::default()
    };

    let start = Instant::now();
    let mut writer = FstHierarchyWriter::new(out, options)?;
    let design = declare(&mut writer)?;
    let mut values = writer.into_values()?;
    let declared = start.elapsed();

    let pattern = generate(&design);

    let start = Instant::now();
    let changes = replay(&mut values, &design, &pattern, args.steps)?;
    let recorded = start.elapsed();
    let mut out = values.finish()?;
    let total = start.elapsed();
    let len = out.seek(SeekFrom::End(0))?;

    println!(
        "signals: {} ({} with aliases), steps: {}, changes: {changes}",
        design.handles.len() + 1,
        design.handles.len() + 1 + design.handles.len().div_ceil(16),
        args.steps
    );
    println!(
        "threads: {}, block size: {} MiB, dedup: {}",
        args.threads,
        args.block_size >> 20,
        args.deduplicate
    );
    println!("declare + hierarchy: {:.3}s", declared.as_secs_f64());
    println!("record changes:      {:.3}s", recorded.as_secs_f64());
    println!(
        "finish:              {:.3}s",
        (total - recorded).as_secs_f64()
    );
    println!(
        "values total:        {:.3}s ({:.1} M changes/s)",
        total.as_secs_f64(),
        changes as f64 / total.as_secs_f64() / 1e6
    );
    println!("output: {:.1} MiB", len as f64 / (1 << 20) as f64);
    if let Some(peak) = peak_rss_kib() {
        println!("peak RSS: {:.0} MiB", peak as f64 / 1024.0);
    }
    Ok(len)
}

/// The process's peak resident set size, from `/proc/self/status` on Linux.
fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn main() -> std::io::Result<()> {
    let args = parse_args();
    match &args.output {
        Some(path) => {
            let file = std::io::BufWriter::new(std::fs::File::create(path)?);
            run(file, &args)?;
        }
        None => {
            run(Discard::default(), &args)?;
        }
    }
    Ok(())
}
