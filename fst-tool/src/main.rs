use std::io::{BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};
use fst_io::*;

#[derive(Parser)]
#[command(version, about = "Tools to inspect and edit FST waveform files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the structure of a file.
    Dump {
        /// The FST file to read.
        input: PathBuf,
    },
    /// Print information about a file.
    ///
    /// At least one of the options must be given, otherwise this help is
    /// printed.
    Info {
        /// The FST file to read.
        input: PathBuf,
        /// Print the full hierarchy: scopes and variables.
        #[arg(long)]
        hier: bool,
        /// Print each variable on its own line with its full dotted path,
        /// instead of an indented tree.
        #[arg(long, requires = "hier")]
        flat: bool,
        /// Print only the names: the scope and variable names in the tree, or
        /// the full dotted paths of the variables with --flat.
        #[arg(long, requires = "hier")]
        names: bool,
    },
    /// Convert a file to VCD, optionally only a slice of the time range.
    ///
    /// The whole hierarchy is always written. Only the value changes whose
    /// time is within the range are dumped; the state at the start of the
    /// range is written as the initial `$dumpvars`.
    Vcd {
        /// The FST file to read.
        input: PathBuf,
        /// The VCD file to write. Defaults to standard output.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// First time to dump, in units of the file timescale. Defaults to the
        /// start of the file.
        #[arg(long)]
        start_time: Option<u64>,
        /// Last time to dump (inclusive). Defaults to the end of the file.
        #[arg(long)]
        end_time: Option<u64>,
        /// Number of worker threads used to decode value-change blocks.
        #[arg(long, short, default_value_t = 3)]
        jobs: usize,
    },
    /// Copy a file, keeping only the value change blocks that overlap a time
    /// range.
    ///
    /// The header (updated), geometry, hierarchy and any other section are
    /// preserved.
    Splice {
        /// The FST file to read.
        input: PathBuf,
        /// Start of the time range, in units of the file timescale.
        start_time: u64,
        /// End of the time range (inclusive), in units of the file timescale.
        end_time: u64,
        /// The FST file to write.
        output: PathBuf,
    },
}

fn main() -> std::io::Result<()> {
    match Cli::parse().command {
        Command::Dump { input } => dump(&input),
        Command::Info { hier: false, .. } => {
            // Like readelf and objdump, ask for something to be printed.
            Cli::command()
                .find_subcommand_mut("info")
                .unwrap()
                .print_help()?;
            std::process::exit(2);
        }
        Command::Info {
            input, flat, names, ..
        } => info_hierarchy(&input, flat, names),
        Command::Vcd {
            input,
            output,
            start_time,
            end_time,
            jobs,
        } => vcd(&input, output.as_deref(), start_time, end_time, jobs),
        Command::Splice {
            input,
            start_time,
            end_time,
            output,
        } => splice(&input, start_time, end_time, &output),
    }
}

/// Name of a variable type (FST_VT_*), or `None` if unknown.
fn var_type_name(var_type: u8) -> Option<&'static str> {
    const NAMES: [&str; 30] = [
        "event",
        "integer",
        "parameter",
        "real",
        "real_parameter",
        "reg",
        "supply0",
        "supply1",
        "time",
        "tri",
        "triand",
        "trior",
        "trireg",
        "tri0",
        "tri1",
        "wand",
        "wire",
        "wor",
        "port",
        "sparray",
        "realtime",
        "string",
        "bit",
        "logic",
        "int",
        "shortint",
        "longint",
        "byte",
        "enum",
        "shortreal",
    ];
    NAMES.get(usize::from(var_type)).copied()
}

/// Name of a variable direction (FST_VD_*), or `None` if unknown.
fn var_direction_name(direction: u8) -> Option<&'static str> {
    const NAMES: [&str; 6] = ["", "input", "output", "inout", "buffer", "linkage"];
    NAMES.get(usize::from(direction)).copied()
}

/// Name of a scope type (FST_ST_*), or `None` if unknown.
fn scope_type_name(scope_type: u8) -> Option<&'static str> {
    const NAMES: [&str; 22] = [
        "module",
        "task",
        "function",
        "begin",
        "fork",
        "generate",
        "struct",
        "union",
        "class",
        "interface",
        "package",
        "program",
        "vhdl_architecture",
        "vhdl_procedure",
        "vhdl_function",
        "vhdl_record",
        "vhdl_process",
        "vhdl_block",
        "vhdl_for_generate",
        "vhdl_if_generate",
        "vhdl_generate",
        "vhdl_package",
    ];
    NAMES.get(usize::from(scope_type)).copied()
}

fn scope_type_label(scope_type: u8) -> String {
    scope_type_name(scope_type).map_or_else(|| format!("scope={scope_type}"), str::to_string)
}

fn var_type_label(var_type: u8) -> String {
    var_type_name(var_type).map_or_else(|| format!("type={var_type}"), str::to_string)
}

fn direction_label(direction: u8) -> String {
    var_direction_name(direction).map_or_else(|| format!("dir={direction}"), str::to_string)
}

/// Calls `f` for each entry of the hierarchy of the file, as they are decoded.
fn for_each_hierarchy_entry(
    input: &Path,
    mut f: impl FnMut(HierarchyEntry) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut iterator = SectionIterator::new(BufReader::new(std::fs::File::open(input)?));
    while let Some(section) = iterator.next()? {
        if !matches!(
            section.block_type(),
            Some(BlockType::Hierarchy | BlockType::HierarchyLZ4 | BlockType::HierarchyLZ4Duo)
        ) {
            continue;
        }
        for entry in section.read_hierarchy()?.entries()? {
            f(entry?)?;
        }
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "file has no hierarchy block",
    ))
}

/// Width of each column of the hierarchy listing. A column that is empty on
/// every line has width 0, and is left out.
#[derive(Default)]
struct Widths {
    /// Digits of the largest variable id.
    id: usize,
    /// Whether any variable is an alias, which adds a `*` marker to the id.
    alias: bool,
    direction: usize,
    /// Shared by variable types and scope types.
    type_: usize,
}

impl Widths {
    fn id_column(&self) -> usize {
        match self.id {
            0 => 0,
            id => id + usize::from(self.alias),
        }
    }
}

/// Appends `text`, padded to `width`, and a separator. Does nothing if the
/// column has no width.
fn push_column(line: &mut String, text: &str, width: usize) {
    if width > 0 {
        line.push_str(&format!("{text:<width$} "));
    }
}

/// Prints the hierarchy of the file, as an indented tree or one variable per
/// line.
///
/// The hierarchy is read twice: first to find the width of each column, then
/// to print it. Entries are decoded as they are printed, never all kept.
/// With `names`, only the names are printed.
fn info_hierarchy(input: &Path, flat: bool, names: bool) -> std::io::Result<()> {
    let mut widths = Widths::default();
    if !names {
        for_each_hierarchy_entry(input, |entry| {
            match entry {
                HierarchyEntry::Scope { scope_type, .. } if !flat => {
                    widths.type_ = widths.type_.max(scope_type_label(scope_type).len());
                }
                HierarchyEntry::Var {
                    var_type,
                    direction,
                    id,
                    is_alias,
                    ..
                } => {
                    widths.id = widths.id.max(id.to_string().len());
                    widths.alias |= is_alias;
                    widths.direction = widths.direction.max(direction_label(direction).len());
                    widths.type_ = widths.type_.max(var_type_label(var_type).len());
                }
                _ => {}
            }
            Ok(())
        })?;
    }

    let mut out = BufWriter::new(std::io::stdout().lock());
    // Names of the open scopes.
    let mut scopes: Vec<String> = Vec::new();
    for_each_hierarchy_entry(input, |entry| {
        // In the tree the whole line is indented, not only the name.
        let indent = if flat { 0 } else { scopes.len() * 2 };
        let mut line = " ".repeat(indent);
        match entry {
            HierarchyEntry::Scope {
                scope_type, name, ..
            } => {
                let name = String::from_utf8_lossy(&name).into_owned();
                if !flat {
                    if !names {
                        // The columns of a variable are left blank, except
                        // for the scope type that takes the place of the
                        // variable type.
                        push_column(&mut line, "", widths.id_column());
                        push_column(&mut line, "", widths.direction);
                        push_column(&mut line, &scope_type_label(scope_type), widths.type_);
                    }
                    line.push_str(&name);
                    writeln!(out, "{line}")?;
                }
                scopes.push(name);
            }
            HierarchyEntry::UpScope => {
                scopes.pop();
            }
            HierarchyEntry::Var {
                var_type,
                direction,
                name,
                length,
                id,
                is_alias,
            } => {
                let name = String::from_utf8_lossy(&name);
                if !names {
                    let marker = match is_alias {
                        true => "*",
                        false if widths.alias => " ",
                        false => "",
                    };
                    push_column(&mut line, &format!("{id}{marker}"), widths.id_column());
                    push_column(&mut line, &direction_label(direction), widths.direction);
                    push_column(&mut line, &var_type_label(var_type), widths.type_);
                }
                if flat {
                    for scope in &scopes {
                        line.push_str(scope);
                        line.push('.');
                    }
                }
                line.push_str(&name);
                // The length is implied by a scalar, and by the range in an
                // array name.
                if !names && length > 1 && !name.contains('[') {
                    line.push_str(&format!(" ({length})"));
                }
                writeln!(out, "{line}")?;
            }
            HierarchyEntry::AttributeBegin { .. } | HierarchyEntry::AttributeEnd => {}
        }
        Ok(())
    })?;
    out.flush()
}

fn is_value_change(block_type: Option<BlockType>) -> bool {
    matches!(
        block_type,
        Some(
            BlockType::ValueChangeData
                | BlockType::ValueChangeDataDynamicAlias
                | BlockType::ValueChangeDataDynamicAlias2
        )
    )
}

fn splice(input: &Path, start_time: u64, end_time: u64, output: &Path) -> std::io::Result<()> {
    if start_time > end_time {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "start time is after end time",
        ));
    }

    let open = || -> std::io::Result<_> { Ok(BufReader::new(std::fs::File::open(input)?)) };

    // The header comes first but needs to know which blocks are kept, so the
    // file is read twice. The first pass only reads the 24 fixed bytes of each
    // value change block.
    let mut header = None;
    // Value change blocks are in time order, so the blocks that overlap the
    // range are contiguous: only the first and the last one are needed.
    // `first` and `last` are `(index, start_time, end_time)`.
    let mut first = None;
    let mut last = None;
    let mut total = 0usize;
    let mut iterator = SectionIterator::new(open()?);
    while let Some(section) = iterator.next()? {
        match section.block_type() {
            Some(BlockType::Header) => header = Some(section.read_header()?),
            block_type if is_value_change(block_type) => {
                let block = section.read_value_changes()?;
                if block.end_time >= start_time && block.start_time <= end_time {
                    let entry = (total, block.start_time, block.end_time);
                    first.get_or_insert(entry);
                    last = Some(entry);
                }
                total += 1;
            }
            _ => {}
        }
    }
    let mut header = header.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "file has no header block")
    })?;
    let kept_range = match (first, last) {
        (Some((first, start, _)), Some((last, _, end))) => {
            header.start_time = start;
            header.end_time = end;
            first..last + 1
        }
        _ => {
            header.start_time = start_time;
            header.end_time = start_time;
            0..0
        }
    };
    let kept = kept_range.len();
    header.num_vc_blocks = kept as u64;

    let mut out = BufWriter::new(std::fs::File::create(output)?);
    let mut index = 0usize;
    let mut iterator = SectionIterator::new(open()?);
    while let Some(mut section) = iterator.next()? {
        match section.block_type() {
            Some(BlockType::Header) => {
                write_section_header(&mut out, section.block_type, section.len)?;
                out.write_all(&header.to_bytes())?;
            }
            block_type if is_value_change(block_type) => {
                if kept_range.contains(&index) {
                    section.copy_to(&mut out)?;
                }
                index += 1;
            }
            _ => section.copy_to(&mut out)?,
        }
    }
    out.flush()?;

    println!("Kept {kept} of {total} value change blocks");
    Ok(())
}

/// FST_VT_VCD_PORT: the length field is a raw value, not a bit count.
const VAR_TYPE_PORT: u8 = 18;

/// Builds an `InvalidData` I/O error with a static message.
fn invalid_data(msg: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Variable types whose VCD length is fixed at 64 bits (an f64 real).
fn is_real_var(var_type: u8) -> bool {
    // real, real_parameter, realtime.
    matches!(var_type, 3 | 4 | 20)
}

/// FST_VT_SV_SHORTREAL: a 32-bit real.
const VAR_TYPE_SHORTREAL: u8 = 29;

/// Encodes an FST variable id as a VCD identifier code: base 94 starting at
/// '!', least significant digit first. The id is 1-based (0 is illegal).
///
/// This matches `fstVcdID` in the reference C library.
fn vcd_id(mut id: u64) -> String {
    let mut code = String::new();
    while id != 0 {
        id -= 1;
        code.push((b'!' + (id % 94) as u8) as char);
        id /= 94;
    }
    code
}

/// Renders the FST `timescale` exponent as a VCD time unit, e.g. `1ns` or
/// `100ps`. Matches the mapping in the reference C library.
fn timescale_label(timescale: i8) -> String {
    // The exponent splits into a decade within a unit (10^0, 10^1, 10^2) and a
    // unit (s, ms, us, ns, ps, fs, as, zs). An exponent above 2 or below the
    // smallest unit is clamped the way the reference does, to seconds or to
    // the smallest unit.
    let (scale, unit) = match timescale {
        e if e >= 2 => (100, ""),
        1 => (10, ""),
        0 => (1, ""),
        -1 => (100, "m"),
        -2 => (10, "m"),
        -3 => (1, "m"),
        -4 => (100, "u"),
        -5 => (10, "u"),
        -6 => (1, "u"),
        -7 => (100, "n"),
        -8 => (10, "n"),
        -9 => (1, "n"),
        -10 => (100, "p"),
        -11 => (10, "p"),
        -12 => (1, "p"),
        -13 => (100, "f"),
        -14 => (10, "f"),
        -15 => (1, "f"),
        -16 => (100, "a"),
        -17 => (10, "a"),
        -18 => (1, "a"),
        -19 => (100, "z"),
        -20 => (10, "z"),
        _ => (1, "z"),
    };
    format!("{scale}{unit}s")
}

/// A null-terminated fixed field, as a lossy string with trailing content
/// after the first NUL dropped.
fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Per-variable information needed to write value changes: the VCD identifier
/// code, the bit length from the geometry block, and whether it is a port
/// (which prefixes vectors with `p` instead of `b`).
struct VcdVar {
    code: String,
    length: VariableLength,
    is_port: bool,
}

/// Formats an f64 real the way the reference C library does, with `%.16g`.
fn format_real(bytes: [u8; 8], endianness: RealEndianess) -> String {
    let value = match endianness {
        RealEndianess::LittleEndian => f64::from_le_bytes(bytes),
        RealEndianess::BigEndian => f64::from_be_bytes(bytes),
    };
    // `%.16g` prints up to 16 significant digits, trimming trailing zeros.
    format_g(value, 16)
}

/// A minimal `%.*g` formatter: `precision` significant digits, shortest of
/// fixed and scientific, trailing zeros trimmed. Dependency-free.
fn format_g(value: f64, precision: usize) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if !value.is_finite() {
        // VCD has no representation for these; emit a readable token.
        return format!("{value}");
    }
    let precision = precision.max(1);
    let exp = value.abs().log10().floor() as i32;
    // %g uses scientific notation when the exponent is < -4 or >= precision.
    if exp < -4 || exp >= precision as i32 {
        let mut s = format!("{:.*e}", precision - 1, value);
        trim_mantissa_in_exp(&mut s);
        s
    } else {
        let decimals = (precision as i32 - 1 - exp).max(0) as usize;
        let s = format!("{value:.decimals$}");
        trim_trailing_zeros(s)
    }
}

/// Trims trailing zeros (and a dangling decimal point) from a fixed-notation
/// number.
fn trim_trailing_zeros(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    let trimmed = s.trim_end_matches('0');
    trimmed.trim_end_matches('.').to_string()
}

/// Trims trailing zeros from the mantissa of a Rust `{:e}` string, and drops a
/// `+` is never emitted by Rust so only the mantissa needs fixing.
fn trim_mantissa_in_exp(s: &mut String) {
    if let Some(e) = s.find('e') {
        let (mantissa, exp) = s.split_at(e);
        let mantissa = trim_trailing_zeros(mantissa.to_string());
        *s = format!("{mantissa}{exp}");
    }
}

/// Converts an FST file to VCD, dumping only value changes whose time is
/// within `[start_time, end_time]` (defaulting to the whole file).
fn vcd(
    input: &Path,
    output: Option<&Path>,
    start_time: Option<u64>,
    end_time: Option<u64>,
    jobs: usize,
) -> std::io::Result<()> {
    if let (Some(start), Some(end)) = (start_time, end_time)
        && start > end
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "start time is after end time",
        ));
    }

    let open = || -> std::io::Result<_> { Ok(BufReader::new(std::fs::File::open(input)?)) };

    // First pass: read the header and geometry. The geometry comes after the
    // value change blocks, but the lengths are needed to decode them, so the
    // file is read in two passes.
    let mut header = None;
    let mut lengths = None;
    let mut iterator = SectionIterator::new(open()?);
    while let Some(section) = iterator.next()? {
        match section.block_type() {
            Some(BlockType::Header) => header = Some(section.read_header()?),
            Some(BlockType::Geometry) => {
                lengths = Some(
                    section
                        .read_geometry()?
                        .lengths()
                        .collect::<std::io::Result<Vec<_>>>()?,
                );
            }
            _ => {}
        }
    }
    let header = header.ok_or_else(|| invalid_data("file has no header block"))?;
    let lengths = lengths.ok_or_else(|| invalid_data("file has no geometry block"))?;

    let start = start_time.unwrap_or(header.start_time);
    let end = end_time.unwrap_or(header.end_time);

    let mut out: Box<dyn Write> = match output {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut out = BufWriter::new(&mut *out);

    // Second pass over the hierarchy: write the VCD header and declarations,
    // and build the per-variable table indexed by variable id.
    write_vcd_header(&mut out, &header)?;
    let vars = write_vcd_declarations(&mut out, input, &lengths)?;
    writeln!(out, "$enddefinitions $end")?;

    write_vcd_value_changes(&mut out, input, &lengths, &vars, &header, start, end, jobs)?;

    out.flush()
}

/// Writes the `$date`, `$version`, `$timezero` and `$timescale` sections.
fn write_vcd_header<W: Write>(out: &mut W, header: &HeaderBlock) -> std::io::Result<()> {
    // asctime() leaves a trailing newline inside the field; the reference
    // writes the date verbatim, so `$date\n\t<date>\n$end` keeps that newline
    // and produces the blank line before `$end`.
    writeln!(out, "$date\n\t{}\n$end", c_string(&header.date))?;
    writeln!(out, "$version\n\t{}\n$end", c_string(&header.writer))?;
    if header.timezero != 0 {
        writeln!(out, "$timezero\n\t{}\n$end", header.timezero)?;
    }
    writeln!(
        out,
        "$timescale\n\t{}\n$end",
        timescale_label(header.timescale)
    )
}

/// Writes the `$scope`/`$var`/`$upscope` declarations, returning a map from
/// variable id to its VCD metadata. Every declared variable (including
/// aliases) gets a `$var` line sharing the aliased id's code.
fn write_vcd_declarations<W: Write>(
    out: &mut W,
    input: &Path,
    lengths: &[VariableLength],
) -> std::io::Result<std::collections::HashMap<u64, VcdVar>> {
    let mut vars: std::collections::HashMap<u64, VcdVar> = std::collections::HashMap::new();
    // The geometry lengths are indexed by non-aliased variable id - 1.
    let mut next_length = 0usize;
    let mut unnamed_scope = 0u32;

    for_each_hierarchy_entry(input, |entry| {
        match entry {
            HierarchyEntry::Scope {
                scope_type, name, ..
            } => {
                let type_name = scope_type_name(scope_type).unwrap_or("module");
                let name = String::from_utf8_lossy(&name).into_owned();
                if name.is_empty() {
                    writeln!(
                        out,
                        "$scope {type_name} $unnamed_scope_{unnamed_scope} $end"
                    )?;
                    unnamed_scope += 1;
                } else {
                    writeln!(out, "$scope {type_name} {name} $end")?;
                }
            }
            HierarchyEntry::UpScope => writeln!(out, "$upscope $end")?,
            HierarchyEntry::Var {
                var_type,
                name,
                length,
                id,
                is_alias,
                direction: _,
            } => {
                let name = String::from_utf8_lossy(&name).into_owned();
                let type_name = var_type_name(var_type).unwrap_or("wire");
                // The VCD identifier code is 1-based; fst-io ids are 0-based.
                let code = vcd_id(id + 1);
                // The VCD length: ports pack three VCD chars per bit, reals are
                // fixed width, everything else is the bit length.
                let vcd_length = if var_type == VAR_TYPE_PORT {
                    length.saturating_sub(2) / 3
                } else if is_real_var(var_type) {
                    64
                } else if var_type == VAR_TYPE_SHORTREAL {
                    32
                } else {
                    length
                };
                writeln!(out, "$var {type_name} {vcd_length} {code} {name} $end")?;

                // A non-aliased variable consumes the next geometry length and
                // becomes the id's canonical decode info. An alias reuses the
                // aliased id's entry (already recorded or recorded later in
                // declaration order, which always precedes its aliases).
                if !is_alias {
                    let geometry_length = lengths
                        .get(next_length)
                        .copied()
                        .ok_or_else(|| invalid_data("more variables than geometry lengths"))?;
                    next_length += 1;
                    vars.entry(id).or_insert(VcdVar {
                        code,
                        length: geometry_length,
                        is_port: var_type == VAR_TYPE_PORT,
                    });
                }
            }
            HierarchyEntry::AttributeBegin { .. } | HierarchyEntry::AttributeEnd => {}
        }
        Ok(())
    })?;

    Ok(vars)
}

/// Dumps the value changes within `[start, end]`.
///
/// FST value-change blocks are time-ordered and disjoint: every time in block
/// N precedes every time in block N+1. So the whole dump is already in time
/// order block-by-block, and the decoding of one in-range block does not
/// depend on any other. That independence is what lets the decode run in
/// parallel: the main thread streams buffered blocks to a pool of worker
/// threads, each turns a block into a time-sorted change list, and a single
/// writer thread reassembles the lists in block order and emits the VCD text.
///
/// Only the decode is parallel; the output is still one serial, time-ordered
/// stream. The number of blocks in flight (dispatched to the pool but not yet
/// written) is bounded, which bounds memory regardless of file size -- the
/// same streaming guarantee the serial version had.
///
/// The prelude -- folding every block before the range, and the first in-range
/// block's initial state, into the `$dumpvars` baseline -- stays on the main
/// thread: it is inherently serial (a running fold toward `start`) and is
/// written before any worker output. The `$dumpvars`/`#time` state machine
/// likewise lives only on the writer thread, so the emitted framing mirrors the
/// reference C library exactly, as in the serial path.
#[allow(clippy::too_many_arguments)]
fn write_vcd_value_changes<W: Write>(
    out: &mut W,
    input: &Path,
    lengths: &[VariableLength],
    vars: &std::collections::HashMap<u64, VcdVar>,
    header: &HeaderBlock,
    start: u64,
    end: u64,
    threads: usize,
) -> std::io::Result<()> {
    let real_endianness = header.real_endianness;

    // Variable ids in geometry order, so the bits array can be mapped to ids.
    let mut ids: Vec<u64> = Vec::new();
    for_each_hierarchy_entry(input, |entry| {
        if let HierarchyEntry::Var { id, is_alias, .. } = entry
            && !is_alias
        {
            ids.push(id);
        }
        Ok(())
    })?;

    // The state of each variable at the start of the dumped range, built from
    // the initial bits array and pre-range changes of every block up to and
    // including the first dumped one. Indexed by (0-based) non-aliased id.
    let mut baseline: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
    let mut baseline_real: std::collections::HashMap<u64, [u8; 8]> =
        std::collections::HashMap::new();

    // Set true if any block entirely before the range was skipped.
    let mut blocks_skipped = false;

    let mut iterator = SectionIterator::new(std::fs::File::open(input)?);

    // --- Serial prelude: fold pre-range blocks and locate the first in-range
    // block, so the baseline carried into the range is complete before any
    // parallel work starts. ---
    let mut first_in_range: Option<Section<std::io::Cursor<Vec<u8>>>> = None;
    while let Some(section) = iterator.next()? {
        if !is_value_change(section.block_type()) {
            continue;
        }
        // Buffer the block so it is detached from the file iterator and can be
        // decoded here (prelude) or moved to a worker thread (in-range).
        let section = section.buffer()?;
        let mut block = section.read_value_changes()?;

        if block.end_time < start {
            // Before the range: fold its final state into the baseline. The
            // bits array is the state at the block's own start, and its changes
            // carry it forward, so the latest value of each variable wins.
            let head = block.read_head_layout()?;
            let tail = block.read_tail_layout()?;
            let positions = block.read_positions(&head, &tail)?;
            let bits = block.read_bits(&head)?;
            for (index, value) in bits.values(lengths).enumerate() {
                let value = value?;
                if let Some(&id) = ids.get(index) {
                    update_baseline(&mut baseline, &mut baseline_real, id, &value);
                }
            }
            for (geometry_index, &id) in ids.iter().enumerate() {
                let waves = block.read_waves(&head, &positions, id, lengths[geometry_index])?;
                for change in waves.iter() {
                    let (_, value) = change?;
                    update_baseline(&mut baseline, &mut baseline_real, id, &value);
                }
            }
            blocks_skipped = true;
            continue;
        }
        if block.start_time > end {
            // Entirely past the range: nothing in range follows.
            break;
        }

        // First in-range block: fold its initial bits array and its pre-`start`
        // changes into the baseline, completing the state carried into `start`.
        let head = block.read_head_layout()?;
        let bits = block.read_bits(&head)?;
        for (index, value) in bits.values(lengths).enumerate() {
            let value = value?;
            if let Some(&id) = ids.get(index) {
                update_baseline(&mut baseline, &mut baseline_real, id, &value);
            }
        }
        if start > block.start_time {
            // Changes before `start` advance the baseline rather than emit.
            let tail = block.read_tail_layout()?;
            let positions = block.read_positions(&head, &tail)?;
            let times = block.read_time_table(&tail)?;
            for (geometry_index, &id) in ids.iter().enumerate() {
                let waves = block.read_waves(&head, &positions, id, lengths[geometry_index])?;
                for change in waves.iter() {
                    let (time_index, value) = change?;
                    let time = times
                        .get(time_index as usize)
                        .copied()
                        .ok_or_else(|| invalid_data("time index out of range"))?;
                    if time < start {
                        update_baseline(&mut baseline, &mut baseline_real, id, &value);
                    }
                }
            }
        }

        // Re-buffer the block to hand to a worker: the prelude read moved its
        // cursor, and the worker decodes it from scratch. Rebuilding from the
        // owned buffer avoids re-reading the file.
        first_in_range = Some(rebuffer(block));
        break;
    }

    // --- Emit the initial frame (if needed) before any value changes. ---
    let frame = match &first_in_range {
        Some(section) => {
            let block_start = value_change_start_time(section)?;
            let first_time = value_change_first_time(section)?.unwrap_or(block_start);
            // A separate initial frame is written when the dump does not begin
            // exactly at the first block's first time step: a later start time,
            // a skipped block, or a block whose first time is after its start.
            blocks_skipped || start > first_time || block_start != first_time
        }
        None => false,
    };
    if frame {
        let frame_time = first_in_range
            .as_ref()
            .map(value_change_start_time)
            .transpose()?
            .map(|s| s.max(start))
            .unwrap_or(start);
        writeln!(out, "#{frame_time}")?;
        writeln!(out, "$dumpvars")?;
        write_baseline(out, &ids, vars, &baseline, &baseline_real, real_endianness)?;
        writeln!(out, "$end")?;
    }
    // The dumpvars state machine starts closed when a frame was emitted (it
    // already played the dumpvars role), otherwise not started.
    let initial_dumpvars_state = if frame { 2u8 } else { 0u8 };
    let initial_time = if frame {
        first_in_range
            .as_ref()
            .map(value_change_start_time)
            .transpose()?
            .map(|s| s.max(start))
    } else {
        None
    };

    // If there is no in-range block, there is nothing to parallelize: a frame
    // may still have been emitted above, with no changes to follow.
    let Some(first_block) = first_in_range else {
        return Ok(());
    };

    // --- Parallel decode: workers decode blocks, the writer emits them in
    // order. ---
    let threads = threads.max(1);
    let in_flight_cap = threads + 1;

    // Bounds blocks dispatched-but-not-yet-written to `in_flight_cap`: the main
    // thread acquires a permit before dispatching a block, the writer releases
    // one after writing it.
    let in_flight = Semaphore::new(in_flight_cap);

    // Work queue (main -> workers) and result queue (workers -> writer).
    let (work_tx, work_rx) = std::sync::mpsc::channel::<Job>();
    let work_rx = std::sync::Mutex::new(work_rx);
    let (result_tx, result_rx) = std::sync::mpsc::channel::<std::io::Result<Decoded>>();

    // The writer runs on THIS (main) thread because `out` is not `Send`. Workers
    // and the dispatcher are scoped threads; they borrow the shared read-only
    // metadata (`ids`, `lengths`, `vars` via the writer) and the semaphore.
    std::thread::scope(|scope| -> std::io::Result<()> {
        // Worker threads: pull a job, decode its block, send the result.
        for _ in 0..threads {
            let work_rx = &work_rx;
            let result_tx = result_tx.clone();
            let ids = &ids;
            scope.spawn(move || {
                loop {
                    let job = {
                        let guard = work_rx.lock().unwrap();
                        guard.recv()
                    };
                    let Ok(job) = job else { break };
                    let decoded =
                        decode_block_changes(job.seq, job.section, ids, lengths, start, end);
                    // A send failure means the writer is gone (an earlier error
                    // tore down the scope); stop quietly.
                    if result_tx.send(decoded).is_err() {
                        break;
                    }
                }
            });
        }

        // Dispatcher thread: feed in-range blocks to the workers in sequence
        // order, blocking on the semaphore so the number of blocks in flight
        // stays bounded. A fatal read error is sent to the writer so the main
        // thread observes it; a shutdown (writer stopped early) aborts dispatch.
        let dispatch_tx = result_tx.clone();
        let in_flight = &in_flight;
        scope.spawn(move || {
            let mut seq = 0u64;
            // Returns false if the semaphore was shut down, meaning stop.
            let mut dispatch = |section: Section<std::io::Cursor<Vec<u8>>>| -> bool {
                if !in_flight.acquire() {
                    return false;
                }
                let _ = work_tx.send(Job { seq, section });
                seq += 1;
                true
            };

            let run = || -> std::io::Result<()> {
                if !dispatch(first_block) {
                    return Ok(());
                }
                while let Some(section) = iterator.next()? {
                    if !is_value_change(section.block_type()) {
                        continue;
                    }
                    let section = section.buffer()?;
                    // Stop once a block starts past the range.
                    if value_change_start_time(&section)? > end {
                        break;
                    }
                    if !dispatch(section) {
                        break;
                    }
                }
                Ok(())
            };
            if let Err(err) = run() {
                let _ = dispatch_tx.send(Err(err));
            }
            // Closing the work channel lets workers finish once drained.
            drop(work_tx);
        });

        // Drop the main thread's spare senders so the result channel closes
        // once every worker and the dispatcher have finished.
        drop(result_tx);

        // Writer loop (main thread): reassemble decoded blocks in sequence
        // order and emit them as one serial VCD stream. The result is captured
        // rather than propagated with `?`, so that on ANY exit the semaphore is
        // shut down and the result receiver dropped -- otherwise the dispatcher
        // could block forever in `acquire` and workers in `recv`, hanging the
        // scope that joins them.
        let mut next_seq = 0u64;
        let mut pending: std::collections::BTreeMap<u64, Vec<(u64, u64, RenderedValue)>> =
            std::collections::BTreeMap::new();
        let mut dumpvars_state = initial_dumpvars_state;
        let mut current_time = initial_time;

        let mut write_loop = || -> std::io::Result<()> {
            while let Ok(result) = result_rx.recv() {
                let decoded = result?;
                pending.insert(decoded.seq, decoded.changes);
                // Emit every block that is now contiguous from next_seq.
                while let Some(changes) = pending.remove(&next_seq) {
                    emit_block_changes(
                        out,
                        &changes,
                        vars,
                        real_endianness,
                        &mut dumpvars_state,
                        &mut current_time,
                    )?;
                    next_seq += 1;
                    // One block written: free a permit so the dispatcher may
                    // send another, keeping in-flight <= 2 * threads.
                    in_flight.release();
                }
            }
            // Close a dumpvars section left open by a single-time-step dump.
            if dumpvars_state == 1 {
                writeln!(out, "$end")?;
            }
            Ok(())
        };
        let result = write_loop();

        // Unblock the dispatcher and workers so the scope can join cleanly,
        // whether the writer finished normally or bailed on an error.
        in_flight.shutdown();
        drop(result_rx);

        result
    })
}

/// A block handed to a worker for decoding, tagged with its position in the
/// dump so the writer can reassemble the results in order.
struct Job {
    seq: u64,
    section: Section<std::io::Cursor<Vec<u8>>>,
}

/// A worker's decoded output: the block's in-range changes, time-sorted.
struct Decoded {
    seq: u64,
    changes: Vec<(u64, u64, RenderedValue)>,
}

/// Decodes one in-range block into its time-sorted change list. This is the
/// parallel hot path: it runs on a worker thread and touches only its own
/// buffered block plus the shared read-only metadata.
fn decode_block_changes(
    seq: u64,
    section: Section<std::io::Cursor<Vec<u8>>>,
    ids: &[u64],
    lengths: &[VariableLength],
    start: u64,
    end: u64,
) -> std::io::Result<Decoded> {
    let mut block = section.read_value_changes()?;
    let head = block.read_head_layout()?;
    let tail = block.read_tail_layout()?;
    let positions = block.read_positions(&head, &tail)?;
    let times = block.read_time_table(&tail)?;

    let mut changes: Vec<(u64, u64, RenderedValue)> = Vec::new();
    for (geometry_index, &id) in ids.iter().enumerate() {
        let waves = block.read_waves(&head, &positions, id, lengths[geometry_index])?;
        for change in waves.iter() {
            let (time_index, value) = change?;
            let time = times
                .get(time_index as usize)
                .copied()
                .ok_or_else(|| invalid_data("time index out of range"))?;
            // Pre-`start` changes were already folded into the baseline by the
            // serial prelude; here they are simply skipped.
            if time < start || time > end {
                continue;
            }
            changes.push((time, id, RenderedValue::from_value(&value)));
        }
    }
    // Stable sort by time keeps per-variable order within a time step.
    changes.sort_by_key(|&(time, _, _)| time);
    Ok(Decoded { seq, changes })
}

/// Reads a buffered value change block's start time without consuming it.
fn value_change_start_time(section: &Section<std::io::Cursor<Vec<u8>>>) -> std::io::Result<u64> {
    let data = section.data.get_ref();
    // The 24 fixed-size fields follow the already-read section header; the
    // start time is the first 8 bytes of the section data.
    let bytes = data
        .get(0..8)
        .ok_or_else(|| invalid_data("value change block too short"))?;
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

/// Reads a buffered block's first time-table entry, used to decide framing.
/// Returns `None` if the block has no time steps.
fn value_change_first_time(
    section: &Section<std::io::Cursor<Vec<u8>>>,
) -> std::io::Result<Option<u64>> {
    // Decoding the time table needs the tail layout; clone the buffer so the
    // caller's section is left untouched for dispatch.
    let clone = Section {
        block_type: section.block_type,
        len: section.len,
        data: std::io::Cursor::new(section.data.get_ref().clone()),
    };
    let mut block = clone.read_value_changes()?;
    let tail = block.read_tail_layout()?;
    let times = block.read_time_table(&tail)?;
    Ok(times.first().copied())
}

/// Rebuilds a buffered section from a value change block whose cursor the
/// prelude advanced, so a worker can decode it from the start without
/// re-reading the file. The block's buffer is the full section data minus the
/// 24-byte fixed header consumed by `read_value_changes`, so the section is
/// reconstructed from the original owned bytes.
fn rebuffer(
    block: ValueChangeBlock<std::io::Cursor<Vec<u8>>>,
) -> Section<std::io::Cursor<Vec<u8>>> {
    let data = block.data.into_inner();
    Section {
        block_type: block.block_type as u8,
        len: block.len,
        data: std::io::Cursor::new(data),
    }
}

/// A counting semaphore built on the standard library only (fst-tool takes no
/// threading dependency). `acquire` blocks until a permit is free or the
/// semaphore is shut down; `release` returns a permit and wakes a waiter.
///
/// `shutdown` exists for the error path: if the writer stops early, the
/// dispatcher could otherwise block forever in `acquire` (no permits are ever
/// released again), which would hang the scope that joins every thread.
/// Shutting down makes every current and future `acquire` return immediately.
struct Semaphore {
    state: std::sync::Mutex<SemaphoreState>,
    available: std::sync::Condvar,
}

struct SemaphoreState {
    permits: usize,
    shutdown: bool,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Semaphore {
            state: std::sync::Mutex::new(SemaphoreState {
                permits,
                shutdown: false,
            }),
            available: std::sync::Condvar::new(),
        }
    }

    /// Blocks for a permit. Returns `true` on success, `false` if the semaphore
    /// was shut down (in which case no permit was taken).
    fn acquire(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        while state.permits == 0 && !state.shutdown {
            state = self.available.wait(state).unwrap();
        }
        if state.shutdown {
            return false;
        }
        state.permits -= 1;
        true
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.permits += 1;
        self.available.notify_one();
    }

    /// Wakes every waiter and makes all future `acquire` calls return `false`.
    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.shutdown = true;
        self.available.notify_all();
    }
}

/// Emits one block's already-sorted changes, carrying the `$dumpvars` state
/// machine and the last-written `#time` across block boundaries so the whole
/// stream reads as one time-ordered dump.
fn emit_block_changes<W: Write>(
    out: &mut W,
    changes: &[(u64, u64, RenderedValue)],
    vars: &std::collections::HashMap<u64, VcdVar>,
    real_endianness: RealEndianess,
    dumpvars_state: &mut u8,
    current_time: &mut Option<u64>,
) -> std::io::Result<()> {
    for (time, id, value) in changes {
        if *current_time != Some(*time) {
            // The reference opens `$dumpvars` at the first dumped time and
            // closes it at the second; on the frame path the state starts
            // closed so neither happens here.
            if *dumpvars_state == 1 {
                writeln!(out, "$end")?;
                *dumpvars_state = 2;
            }
            writeln!(out, "#{time}")?;
            if *dumpvars_state == 0 {
                writeln!(out, "$dumpvars")?;
                *dumpvars_state = 1;
            }
            *current_time = Some(*time);
        }
        if let Some(var) = vars.get(id) {
            value.write(out, var, real_endianness)?;
        }
    }
    Ok(())
}

/// An owned value change, decoupled from the block's borrowed buffers.
enum RenderedValue {
    Bit(u8),
    Real([u8; 8]),
    VarLen(Vec<u8>),
    Vector(Vec<u8>),
}

impl RenderedValue {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Bit(bit) => RenderedValue::Bit(*bit),
            Value::Real(bytes) => RenderedValue::Real(*bytes),
            Value::VarLen(data) => RenderedValue::VarLen(data.to_vec()),
            Value::Packed { .. } | Value::Ascii(_) => RenderedValue::Vector(value.to_ascii()),
        }
    }

    fn write<W: Write>(
        &self,
        out: &mut W,
        var: &VcdVar,
        real_endianness: RealEndianess,
    ) -> std::io::Result<()> {
        match self {
            RenderedValue::Bit(bit) => writeln!(out, "{}{}", *bit as char, var.code),
            RenderedValue::Real(bytes) => {
                writeln!(
                    out,
                    "r{} {}",
                    format_real(*bytes, real_endianness),
                    var.code
                )
            }
            RenderedValue::VarLen(data) => {
                writeln!(out, "s{} {}", String::from_utf8_lossy(data), var.code)
            }
            RenderedValue::Vector(ascii) => {
                let prefix = if var.is_port { 'p' } else { 'b' };
                write!(out, "{prefix}")?;
                out.write_all(ascii)?;
                writeln!(out, " {}", var.code)
            }
        }
    }
}

/// Records a variable's initial-state value into the running baseline maps.
fn update_baseline(
    baseline: &mut std::collections::HashMap<u64, Vec<u8>>,
    baseline_real: &mut std::collections::HashMap<u64, [u8; 8]>,
    id: u64,
    value: &Value,
) {
    match value {
        Value::Real(bytes) => {
            baseline_real.insert(id, *bytes);
        }
        Value::VarLen(_) => {
            // Variable-length values have no initial state in a VCD dump.
        }
        _ => {
            baseline.insert(id, value.to_ascii());
        }
    }
}

/// Writes the `$dumpvars` body: the current value of every scalar, vector and
/// real variable.
fn write_baseline<W: Write>(
    out: &mut W,
    ids: &[u64],
    vars: &std::collections::HashMap<u64, VcdVar>,
    baseline: &std::collections::HashMap<u64, Vec<u8>>,
    baseline_real: &std::collections::HashMap<u64, [u8; 8]>,
    real_endianness: RealEndianess,
) -> std::io::Result<()> {
    for &id in ids {
        let Some(var) = vars.get(&id) else { continue };
        if let Some(bytes) = baseline_real.get(&id) {
            writeln!(
                out,
                "r{} {}",
                format_real(*bytes, real_endianness),
                var.code
            )?;
            continue;
        }
        let Some(ascii) = baseline.get(&id) else {
            continue;
        };
        match var.length {
            VariableLength::Bits(1) => {
                writeln!(
                    out,
                    "{}{}",
                    ascii.first().copied().unwrap_or(b'x') as char,
                    var.code
                )?;
            }
            VariableLength::VarLen => {}
            _ => {
                let prefix = if var.is_port { 'p' } else { 'b' };
                write!(out, "{prefix}")?;
                out.write_all(ascii)?;
                writeln!(out, " {}", var.code)?;
            }
        }
    }
    Ok(())
}

fn dump(path: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);

    // The geometry block comes after the value change blocks, but is needed to
    // decode them, so the file is read twice.
    let mut real_endianness = None;
    let mut geometry_lengths = None;

    let mut iterator = SectionIterator::new(&mut reader);

    while let Some(section) = iterator.next()? {
        println!("kind={} len={}", section.block_type, section.len());
        match section.block_type() {
            Some(BlockType::Header) => {
                let header = section.read_header()?;
                println!("Header: {:?}", header);
                real_endianness = Some(header.real_endianness);
            }
            Some(BlockType::Geometry) => {
                let geometry = section.read_geometry()?;
                println!(
                    "Geometry: count={} uncompressed_length={} compressed_length={}",
                    geometry.count, geometry.uncompressed_length, geometry.compressed_length
                );
                let lengths = geometry.lengths().collect::<std::io::Result<Vec<_>>>()?;
                for (i, length) in lengths.iter().take(10).enumerate() {
                    println!("  [{i}] {:?}", length);
                }
                geometry_lengths = Some(lengths);
            }
            Some(BlockType::Hierarchy | BlockType::HierarchyLZ4 | BlockType::HierarchyLZ4Duo) => {
                let hierarchy = section.read_hierarchy()?;
                println!(
                    "Hierarchy: compression={:?} uncompressed_length={}",
                    hierarchy.compression, hierarchy.uncompressed_length
                );
                let mut depth = 0;
                for entry in hierarchy.entries()?.take(20) {
                    let entry = entry?;
                    if let HierarchyEntry::UpScope = entry {
                        depth -= 1;
                    }
                    println!("  {:indent$}{}", "", entry, indent = depth * 2);
                    if let HierarchyEntry::Scope { .. } = entry {
                        depth += 1;
                    }
                }
            }
            _ => {}
        }
    }

    let (Some(real_endianness), Some(lengths)) = (real_endianness, geometry_lengths) else {
        println!("No header or geometry block, can't read value changes");
        return Ok(());
    };

    reader.rewind()?;
    let mut iterator = SectionIterator::new(&mut reader);
    while let Some(section) = iterator.next()? {
        if !matches!(
            section.block_type(),
            Some(
                BlockType::ValueChangeData
                    | BlockType::ValueChangeDataDynamicAlias
                    | BlockType::ValueChangeDataDynamicAlias2
            )
        ) {
            continue;
        }

        let mut block = section.read_value_changes()?;
        let head = block.read_head_layout()?;
        let tail = block.read_tail_layout()?;
        let positions = block.read_positions(&head, &tail)?;
        println!(
            "ValueChange: type={:?} time={}..{} memory_required={} vars={} pack_type={:?} bits_count={} time_count={}",
            block.block_type,
            block.start_time,
            block.end_time,
            block.memory_required,
            head.waves_count,
            head.pack_type as char,
            head.bits_count,
            tail.time_count,
        );

        let times = block.read_time_table(&tail)?;
        let bits = block.read_bits(&head)?;
        for (id, value) in bits.values(&lengths).take(4).enumerate() {
            println!("  initial [{id}] {}", value?.display(real_endianness));
        }

        for id in 0..lengths.len().min(4) as u64 {
            let waves = block.read_waves(&head, &positions, id, lengths[id as usize])?;
            for change in waves.iter().take(5) {
                let (time_index, value) = change?;
                let time = times[time_index as usize];
                println!("  #{time} [{id}] {}", value.display(real_endianness));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vcd_id_is_base_94_from_one() {
        // Id 1 is the first printable code; 94 wraps into a second digit.
        assert_eq!(vcd_id(1), "!");
        assert_eq!(vcd_id(2), "\"");
        assert_eq!(vcd_id(94), "~");
        assert_eq!(vcd_id(95), "!!");
        assert_eq!(vcd_id(96), "\"!");
    }

    #[test]
    fn timescale_label_maps_exponent_to_unit() {
        assert_eq!(timescale_label(0), "1s");
        assert_eq!(timescale_label(1), "10s");
        assert_eq!(timescale_label(-3), "1ms");
        assert_eq!(timescale_label(-7), "100ns");
        assert_eq!(timescale_label(-9), "1ns");
        assert_eq!(timescale_label(-12), "1ps");
    }

    #[test]
    fn format_g_trims_trailing_zeros_and_matches_c() {
        assert_eq!(format_g(0.0, 16), "0");
        assert_eq!(format_g(1.25, 16), "1.25");
        assert_eq!(format_g(-3.5, 16), "-3.5");
        assert_eq!(format_g(100.0, 16), "100");
        // A value whose exponent reaches the scientific-notation threshold.
        assert_eq!(format_g(1e20, 16), "1e20");
    }

    #[test]
    fn c_string_stops_at_the_first_nul() {
        assert_eq!(c_string(b"fstWriter\0\0\0"), "fstWriter");
        assert_eq!(c_string(b"no terminator"), "no terminator");
    }
}
