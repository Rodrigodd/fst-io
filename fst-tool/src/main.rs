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
        /// Decompress a gzip-wrapped file and dump the inner FST. Without it,
        /// a wrapped file is reported and left untouched, since its block
        /// structure is a single wrapper section.
        #[arg(long)]
        allow_compressed: bool,
    },
    /// Print information about a file.
    ///
    /// At least one of the options must be given, otherwise this help is
    /// printed.
    Info {
        /// The FST file to read.
        input: PathBuf,
        /// Print the header: timescale, time range, writer, date and counts.
        /// Printed before the hierarchy when combined with --hier.
        #[arg(long)]
        header: bool,
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
        /// Only output signals whose full dotted path matches a glob pattern
        /// (`*`, `?`). A pattern matching a scope's path selects every signal
        /// under it. Repeatable, and a single argument may list several
        /// patterns separated by commas. Defaults to every signal.
        #[arg(long = "signal", value_name = "GLOB", value_delimiter = ',')]
        signals: Vec<String>,
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
        /// Splice a gzip-wrapped file, writing an uncompressed FST. Without it,
        /// a wrapped file is refused, since the output would not carry the
        /// wrapper's compression.
        #[arg(long)]
        allow_compressed: bool,
    },
}

fn main() -> std::io::Result<()> {
    match Cli::parse().command {
        Command::Dump {
            input,
            allow_compressed,
        } => dump(&input, allow_compressed),
        Command::Info {
            header: false,
            hier: false,
            ..
        } => {
            // Like readelf and objdump, ask for something to be printed.
            Cli::command()
                .find_subcommand_mut("info")
                .unwrap()
                .print_help()?;
            std::process::exit(2);
        }
        Command::Info {
            input,
            header,
            hier,
            flat,
            names,
        } => info(&input, header, hier, flat, names),
        Command::Vcd {
            input,
            output,
            start_time,
            end_time,
            jobs,
            signals,
        } => vcd(
            &input,
            output.as_deref(),
            start_time,
            end_time,
            jobs,
            &signals,
        ),
        Command::Splice {
            input,
            start_time,
            end_time,
            output,
            allow_compressed,
        } => splice(&input, start_time, end_time, &output, allow_compressed),
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
    input: &FstInput,
    mut f: impl FnMut(HierarchyEntry) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut iterator = SectionIterator::new(input.open()?);
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

/// Prints the requested parts of a file: the header, the hierarchy, or both
/// (header first). At least one of `header`/`hier` is set by the caller.
fn info(input: &Path, header: bool, hier: bool, flat: bool, names: bool) -> std::io::Result<()> {
    let input = FstInput::open_path(input)?;
    if header {
        info_header(&input)?;
    }
    if hier {
        info_hierarchy(&input, flat, names)?;
    }
    Ok(())
}

/// Prints the header as `key: value` lines: timescale (as a VCD unit label),
/// time range, writer, date, and the scope/variable/block counts.
fn info_header(input: &FstInput) -> std::io::Result<()> {
    let mut iterator = SectionIterator::new(input.open()?);
    let mut header = None;
    while let Some(section) = iterator.next()? {
        if matches!(section.block_type(), Some(BlockType::Header)) {
            header = Some(section.read_header()?);
            break;
        }
    }
    let header = header.ok_or_else(|| invalid_data("file has no header block"))?;

    let mut out = BufWriter::new(std::io::stdout().lock());
    // asctime() leaves a trailing newline inside the date field; trim it so the
    // date stays on its own `key: value` line.
    let date = c_string(&header.date);
    let date = date.trim_end();
    writeln!(out, "timescale: {}", timescale_label(header.timescale))?;
    writeln!(out, "start_time: {}", header.start_time)?;
    writeln!(out, "end_time: {}", header.end_time)?;
    writeln!(out, "writer: {}", c_string(&header.writer))?;
    writeln!(out, "date: {date}")?;
    writeln!(out, "scopes: {}", header.num_scopes)?;
    writeln!(out, "variables: {}", header.num_hierarchy_vars)?;
    writeln!(out, "distinct_variables: {}", header.num_vars)?;
    writeln!(out, "value_change_blocks: {}", header.num_vc_blocks)?;
    out.flush()
}

/// Prints the hierarchy of the file, as an indented tree or one variable per
/// line.
///
/// The hierarchy is read twice: first to find the width of each column, then
/// to print it. Entries are decoded as they are printed, never all kept.
/// With `names`, only the names are printed.
fn info_hierarchy(input: &FstInput, flat: bool, names: bool) -> std::io::Result<()> {
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

fn splice(
    input: &Path,
    start_time: u64,
    end_time: u64,
    output: &Path,
    allow_compressed: bool,
) -> std::io::Result<()> {
    if start_time > end_time {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "start time is after end time",
        ));
    }

    let input = FstInput::open_path(input)?;
    if input.is_wrapped() && !allow_compressed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "input is a gzip-wrapped FST; the output would be uncompressed. \
             Pass --allow-compressed to splice it anyway",
        ));
    }
    let open = || input.open();

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

/// Matches `text` against a glob `pattern` where `*` matches any run of
/// characters (including none) and `?` matches exactly one. All other
/// characters match literally. Matching is over bytes, which is correct for
/// the ASCII path separators and keeps non-UTF-8 names working.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    // Iterative backtracking match: `star` remembers the last `*` seen and the
    // text position to resume from if the tentative match fails, giving linear
    // behavior on the common case without recursion.
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                t += 1;
            }
            Some(&c) if c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, t));
                }
                None => return false,
            },
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

/// A set of glob patterns selecting which signals to output. An empty filter
/// selects everything, so the common no-filter path pays nothing.
///
/// A pattern is matched against a variable's full dotted path. A pattern that
/// matches an enclosing scope's dotted path selects every variable under that
/// scope: the filter tests each path and every prefix that ends at a scope
/// boundary.
struct SignalFilter {
    patterns: Vec<Vec<u8>>,
}

impl SignalFilter {
    fn new(patterns: &[String]) -> Self {
        SignalFilter {
            patterns: patterns.iter().map(|p| p.clone().into_bytes()).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether a variable at `path` (its full dotted path) is selected. `scopes`
    /// are the enclosing scope names, so a pattern matching any scope-boundary
    /// prefix selects the variable.
    fn selects(&self, scopes: &[String], name: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let mut path = String::new();
        for scope in scopes {
            path.push_str(scope);
            path.push('.');
            let scope_prefix = &path[..path.len() - 1];
            if self
                .patterns
                .iter()
                .any(|p| glob_match(p, scope_prefix.as_bytes()))
            {
                return true;
            }
        }
        path.push_str(name);
        self.patterns.iter().any(|p| glob_match(p, path.as_bytes()))
    }
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

/// The digit alphabet for minimal filtered codes, in the required allocation
/// order: lowercase, uppercase, digits, then the remaining printable-ASCII VCD
/// identifier characters (the "special" group, in ASCII order). All 94 legal
/// identifier characters appear exactly once.
static MINIMAL_ALPHABET: [u8; 94] = {
    let mut alphabet = [0u8; 94];
    let mut i = 0usize;
    let mut c = b'a';
    while c <= b'z' {
        alphabet[i] = c;
        i += 1;
        c += 1;
    }
    c = b'A';
    while c <= b'Z' {
        alphabet[i] = c;
        i += 1;
        c += 1;
    }
    c = b'0';
    while c <= b'9' {
        alphabet[i] = c;
        i += 1;
        c += 1;
    }
    // The remaining printable ASCII (0x21..=0x7e) that is not alphanumeric.
    c = b'!';
    while c <= b'~' {
        let is_alnum = c.is_ascii_lowercase() || c.is_ascii_uppercase() || c.is_ascii_digit();
        if !is_alnum {
            alphabet[i] = c;
            i += 1;
        }
        c += 1;
    }
    alphabet
};

/// Encodes a 1-based ordinal as a minimal VCD identifier code over
/// [`MINIMAL_ALPHABET`], used when `--signal` renumbers the kept signals. This
/// is bijective base-94 written most-significant digit first, so codes grow in
/// lexical order over the alphabet: `a`, `b`, ... then `aa`, `ab`, ... (not
/// `aa`, `ba`). The ordinal is 1-based (0 is illegal).
fn minimal_vcd_id(mut n: u64) -> String {
    const BASE: u64 = 94;
    let mut digits = Vec::new();
    while n != 0 {
        n -= 1;
        digits.push(MINIMAL_ALPHABET[(n % BASE) as usize]);
        n /= BASE;
    }
    // `digits` holds least-significant first; reverse for MSD-first so the
    // output orders lexically.
    digits.reverse();
    String::from_utf8(digits).expect("alphabet is ASCII")
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
/// (which prefixes vectors with `p`).
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
/// fixed and scientific, trailing zeros trimmed.
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

/// Trims trailing zeros from the mantissa of a Rust `{:e}` string. Rust never
/// emits a `+` in the exponent, so only the mantissa needs fixing.
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
    signals: &[String],
) -> std::io::Result<()> {
    if let (Some(start), Some(end)) = (start_time, end_time)
        && start > end
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "start time is after end time",
        ));
    }

    let input = FstInput::open_path(input)?;
    let open = || input.open();

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
    let filter = SignalFilter::new(signals);
    write_vcd_header(&mut out, &header)?;
    let vars = write_vcd_declarations(&mut out, &input, &lengths, &filter)?;
    writeln!(out, "$enddefinitions $end")?;

    write_vcd_value_changes(
        &mut out,
        &input,
        &lengths,
        &vars,
        &header,
        start,
        end,
        jobs,
        !filter.is_empty(),
    )?;

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

/// A buffered scope used when filtering, so a scope containing no selected
/// variable can be dropped. Its `$scope` line and body (nested declarations)
/// are held until `$upscope`; the frame is flushed to its parent only if a
/// selected variable set `non_empty`.
struct ScopeFrame {
    scope_line: String,
    body: Vec<u8>,
    non_empty: bool,
}

/// Writes the `$scope`/`$var`/`$upscope` declarations, returning a map from
/// variable id to its VCD metadata. Every declared variable (including
/// aliases) gets a `$var` line sharing the aliased id's code.
///
/// When `filter` is non-empty, only variables it selects get a `$var` line, and
/// an id with no selected `$var` line is left `None` so the value-change path
/// renders nothing for it. A scope that ends up containing no selected variable
/// (directly or in a nested scope) is omitted entirely. The geometry length is
/// still consumed for every canonical variable and the id table stays dense, so
/// filtering never shifts the id-to-geometry mapping.
fn write_vcd_declarations<W: Write>(
    out: &mut W,
    input: &FstInput,
    lengths: &[VariableLength],
    filter: &SignalFilter,
) -> std::io::Result<Vec<Option<VcdVar>>> {
    // Indexed directly by (0-based) non-aliased variable id. Ids are dense, so
    // a Vec avoids a per-value-change hash lookup in the hot emit loop; any
    // gap stays `None`.
    let mut vars: Vec<Option<VcdVar>> = Vec::new();
    // The geometry lengths are indexed by non-aliased variable id - 1.
    let mut next_length = 0usize;
    let mut unnamed_scope = 0u32;
    // Enclosing scope names, for matching a variable's full dotted path. Empty
    // unless a filter is active, so the no-filter path pays nothing.
    let mut scopes: Vec<String> = Vec::new();
    // Buffered scope frames, used only when filtering so an empty scope can be
    // dropped: a scope's `$scope` line and body are held here and flushed to
    // the parent (or `out`) only once a selected variable makes it non-empty.
    let mut frames: Vec<ScopeFrame> = Vec::new();
    // Minimal VCD identifier codes for the filtered path: each selected id is
    // assigned the next dense 1-based code in first-appearance order, so the
    // output uses the shortest codes regardless of how sparse the selected ids
    // are in the original space. Unused without a filter.
    let mut minimal_codes: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut next_code = 1u64;

    for_each_hierarchy_entry(input, |entry| {
        match entry {
            HierarchyEntry::Scope {
                scope_type, name, ..
            } => {
                let type_name = scope_type_name(scope_type).unwrap_or("module");
                let name = String::from_utf8_lossy(&name).into_owned();
                let scope_line = if name.is_empty() {
                    let line = format!("$scope {type_name} $unnamed_scope_{unnamed_scope} $end\n");
                    unnamed_scope += 1;
                    line
                } else {
                    format!("$scope {type_name} {name} $end\n")
                };
                if filter.is_empty() {
                    out.write_all(scope_line.as_bytes())?;
                } else {
                    scopes.push(name);
                    frames.push(ScopeFrame {
                        scope_line,
                        body: Vec::new(),
                        non_empty: false,
                    });
                }
            }
            HierarchyEntry::UpScope => {
                if filter.is_empty() {
                    writeln!(out, "$upscope $end")?;
                } else {
                    scopes.pop();
                    // An unbalanced `$upscope` (malformed input) has no frame
                    // to close; ignore it, matching the unfiltered path which
                    // just writes the bare `$upscope`.
                    let Some(frame) = frames.pop() else {
                        return Ok(());
                    };
                    if frame.non_empty {
                        // Flush the scope (and its now-non-empty body) into the
                        // parent frame, or to `out` at the top level.
                        let sink: &mut Vec<u8> = match frames.last_mut() {
                            Some(parent) => {
                                parent.non_empty = true;
                                &mut parent.body
                            }
                            None => {
                                out.write_all(&frame.scope_line.into_bytes())?;
                                out.write_all(&frame.body)?;
                                out.write_all(b"$upscope $end\n")?;
                                return Ok(());
                            }
                        };
                        sink.extend_from_slice(frame.scope_line.as_bytes());
                        sink.extend_from_slice(&frame.body);
                        sink.extend_from_slice(b"$upscope $end\n");
                    }
                }
            }
            HierarchyEntry::Var {
                var_type,
                name,
                length,
                id,
                is_alias,
                direction: _,
            } => {
                let name = String::from_utf8_lossy(&name).into_owned();
                let selected = filter.selects(&scopes, &name);
                let type_name = var_type_name(var_type).unwrap_or("wire");
                // The VCD identifier code is 1-based; fst-io ids are 0-based.
                // When filtering, a selected id is renumbered densely so the
                // emitted codes stay minimal; all `$var` lines of one id (an
                // alias and its canonical) share the one code.
                let code = if filter.is_empty() {
                    vcd_id(id + 1)
                } else if selected {
                    let number = *minimal_codes.entry(id).or_insert_with(|| {
                        let n = next_code;
                        next_code += 1;
                        n
                    });
                    minimal_vcd_id(number)
                } else {
                    // Not emitted; the value is unused.
                    String::new()
                };
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
                if selected {
                    let var_line = format!("$var {type_name} {vcd_length} {code} {name} $end\n");
                    match frames.last_mut() {
                        // A selected variable makes its scope non-empty, which
                        // keeps every enclosing scope on flush.
                        Some(frame) => {
                            frame.non_empty = true;
                            frame.body.extend_from_slice(var_line.as_bytes());
                        }
                        None => out.write_all(var_line.as_bytes())?,
                    }
                }

                let idx = id as usize;

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
                    if idx >= vars.len() {
                        vars.resize_with(idx + 1, || None);
                    }
                    vars[idx].get_or_insert(VcdVar {
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

    // Prune ids no `$var` line selected: the value-change path treats a `None`
    // entry exactly like an undeclared variable and renders nothing for it, so
    // the filter costs nothing in the decode hot loop. A kept id also gets its
    // minimal code stamped in, since the code assigned when its `vars` entry
    // was first built may predate the (minimal) code a later selected alias of
    // the same id established.
    if !filter.is_empty() {
        for (idx, var) in vars.iter_mut().enumerate() {
            match minimal_codes.get(&(idx as u64)) {
                Some(&number) => {
                    if let Some(var) = var {
                        var.code = minimal_vcd_id(number);
                    }
                }
                None => *var = None,
            }
        }
    }

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
/// written) is bounded, which bounds memory regardless of file size.
///
/// The prelude -- folding every block before the range, and the first in-range
/// block's initial state, into the `$dumpvars` baseline -- stays on the main
/// thread: it is inherently serial (a running fold toward `start`) and is
/// written before any worker output. The `$dumpvars`/`#time` state machine
/// likewise lives only on the writer thread, so the emitted framing mirrors the
/// reference C library exactly.
#[allow(clippy::too_many_arguments)]
fn write_vcd_value_changes<W: Write>(
    out: &mut W,
    input: &FstInput,
    lengths: &[VariableLength],
    vars: &[Option<VcdVar>],
    header: &HeaderBlock,
    start: u64,
    end: u64,
    threads: usize,
    filter_active: bool,
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

    let mut iterator = SectionIterator::new(input.open()?);

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
            // Changes before `start` advance the baseline.
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
    let pool_size = threads + 1;

    // A bounded pool of reusable byte/change buffers. It also provides the
    // pipeline's backpressure: the dispatcher takes a bundle before reading a
    // block and the writer returns it after emitting, so at most `pool_size`
    // blocks are ever in flight -- no separate semaphore needed.
    let pool = BufferPool::new(pool_size);

    // Work queue (main -> workers) and result queue (workers -> writer).
    let (work_tx, work_rx) = std::sync::mpsc::channel::<Job>();
    let work_rx = std::sync::Mutex::new(work_rx);
    let (result_tx, result_rx) = std::sync::mpsc::channel::<std::io::Result<Decoded>>();

    // The writer runs on THIS (main) thread because `out` is not `Send`. Workers
    // and the dispatcher are scoped threads; they borrow the shared read-only
    // metadata (`ids`, `lengths`, `vars` via the writer) and the buffer pool.
    let result = std::thread::scope(|scope| -> std::io::Result<()> {
        // Worker threads: pull a job, decode its block, send the result.
        for i in 0..threads {
            let work_rx = &work_rx;
            let result_tx = result_tx.clone();
            let ids = &ids;
            std::thread::Builder::new()
                .name(format!("vcd-decode-{i}"))
                .spawn_scoped(scope, move || {
                    loop {
                        let job = {
                            let guard = work_rx.lock().unwrap();
                            guard.recv()
                        };
                        let Ok(job) = job else { break };
                        let decoded = decode_block_changes(
                            job.seq,
                            job.section,
                            job.changes,
                            job.scratch,
                            job.payloads,
                            job.offsets,
                            job.framed,
                            ids,
                            lengths,
                            vars,
                            real_endianness,
                            start,
                            end,
                            filter_active,
                        );
                        // A send failure means the writer is gone (an earlier error
                        // tore down the scope); stop quietly.
                        if result_tx.send(decoded).is_err() {
                            break;
                        }
                    }
                })?;
        }

        // Dispatcher thread: feed in-range blocks to the workers in sequence
        // order, taking a buffer bundle from the pool first so the number of
        // blocks in flight stays bounded (an empty pool blocks here). A fatal
        // read error is sent to the writer so the main thread observes it; a
        // pool shutdown (writer stopped early) aborts dispatch.
        let dispatch_tx = result_tx.clone();
        let pool = &pool;
        std::thread::Builder::new()
            .name("vcd-dispatch".to_string())
            .spawn_scoped(scope, move || {
                let mut seq = 0u64;
                // Sends a job carrying the recycled changes and scratch
                // buffers. Returns false if the pool was shut down, meaning
                // stop.
                let mut dispatch = |section: Section<std::io::Cursor<Vec<u8>>>,
                                    changes: Vec<Change>,
                                    scratch: Vec<Change>,
                                    payloads: Vec<u8>,
                                    offsets: Vec<u32>,
                                    framed: Vec<u8>|
                 -> bool {
                    let _ = work_tx.send(Job {
                        seq,
                        section,
                        changes,
                        scratch,
                        payloads,
                        offsets,
                        framed,
                    });
                    seq += 1;
                    true
                };

                let run = || -> std::io::Result<()> {
                    // The first block was already buffered by the prelude; take
                    // a bundle only for its recycled changes buffer (its own
                    // bytes become a pool buffer once the writer returns them).
                    let Some(bundle) = pool.take() else {
                        return Ok(());
                    };
                    if !dispatch(
                        first_block,
                        bundle.changes,
                        bundle.scratch,
                        bundle.payloads,
                        bundle.offsets,
                        bundle.framed,
                    ) {
                        return Ok(());
                    }
                    while let Some(section) = iterator.next()? {
                        if !is_value_change(section.block_type()) {
                            continue;
                        }
                        // Reuse a recycled byte buffer for the section bytes.
                        let Some(bundle) = pool.take() else {
                            break;
                        };
                        let section = section.buffer_reusing(bundle.bytes)?;
                        // Stop once a block starts past the range.
                        if value_change_start_time(&section)? > end {
                            break;
                        }
                        if !dispatch(
                            section,
                            bundle.changes,
                            bundle.scratch,
                            bundle.payloads,
                            bundle.offsets,
                            bundle.framed,
                        ) {
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
            })?;

        // Drop the main thread's spare senders so the result channel closes
        // once every worker and the dispatcher have finished.
        drop(result_tx);

        // Writer loop (main thread): reassemble decoded blocks in sequence
        // order and emit them as one serial VCD stream. The result is captured
        // rather than propagated with `?`, so that on ANY exit the pool is shut
        // down and the result receiver dropped -- otherwise the dispatcher
        // could block forever in `take` and workers in `recv`, hanging the
        // scope that joins them.
        let mut next_seq = 0u64;
        let mut pending: std::collections::BTreeMap<u64, Decoded> =
            std::collections::BTreeMap::new();
        let mut dumpvars_state = initial_dumpvars_state;
        let mut current_time = initial_time;
        // Reusable per-block output buffer: a block's framing + pre-rendered
        // change bytes are composed here, then flushed in one `write_all`.
        let mut line: Vec<u8> = Vec::new();

        let mut write_loop = || -> std::io::Result<()> {
            while let Ok(result) = result_rx.recv() {
                let decoded = result?;
                pending.insert(decoded.seq, decoded);
                // Emit every block that is now contiguous from next_seq.
                while let Some(decoded) = pending.remove(&next_seq) {
                    emit_block_changes(
                        out,
                        &decoded.changes,
                        &decoded.framed,
                        &mut line,
                        &mut dumpvars_state,
                        &mut current_time,
                    )?;
                    next_seq += 1;
                    // One block written: return its buffers to the pool so the
                    // dispatcher may read another, keeping in-flight bounded.
                    pool.give(Buffers {
                        bytes: decoded.bytes,
                        changes: decoded.changes,
                        scratch: decoded.scratch,
                        payloads: decoded.payloads,
                        offsets: decoded.offsets,
                        framed: decoded.framed,
                    });
                }
            }
            // Close a dumpvars section left open by a single-time-step dump.
            if dumpvars_state == 1 {
                out.write_all(b"$end\n")?;
            }
            Ok(())
        };
        let result = write_loop();

        // Unblock the dispatcher and workers so the scope can join cleanly,
        // whether the writer finished normally or bailed on an error.
        pool.shutdown();
        drop(result_rx);

        result
    });

    // Freeing every recycled buffer here would run the whole deallocation on
    // the way out; leak the pool instead and let the process reclaim it.
    std::mem::forget(pool);

    result
}

/// A block handed to a worker for decoding, tagged with its position in the
/// dump so the writer can reassemble the results in order. `changes` is an
/// empty, capacity-retaining vector recycled from the pool for the worker to
/// fill.
struct Job {
    seq: u64,
    section: Section<std::io::Cursor<Vec<u8>>>,
    changes: Vec<Change>,
    scratch: Vec<Change>,
    payloads: Vec<u8>,
    offsets: Vec<u32>,
    framed: Vec<u8>,
}

/// A worker's decoded output: the block's in-range changes, time-sorted, plus
/// the section byte buffer recovered from the consumed block. Both travel back
/// to the writer, which recycles them into the pool after emitting.
struct Decoded {
    seq: u64,
    changes: Vec<Change>,
    bytes: Vec<u8>,
    scratch: Vec<Change>,
    payloads: Vec<u8>,
    offsets: Vec<u32>,
    framed: Vec<u8>,
}

/// Decodes one in-range block into its time-sorted change list. This is the
/// parallel hot path: it runs on a worker thread and touches only its own
/// buffered block plus the shared read-only metadata. The `changes` buffer is
/// reused (cleared first); the section's backing bytes are recovered and
/// returned for recycling.
#[allow(clippy::too_many_arguments)]
fn decode_block_changes(
    seq: u64,
    section: Section<std::io::Cursor<Vec<u8>>>,
    mut changes: Vec<Change>,
    mut scratch: Vec<Change>,
    mut payloads: Vec<u8>,
    mut offsets: Vec<u32>,
    mut framed: Vec<u8>,
    ids: &[u64],
    lengths: &[VariableLength],
    vars: &[Option<VcdVar>],
    real_endianness: RealEndianess,
    start: u64,
    end: u64,
    filter_active: bool,
) -> std::io::Result<Decoded> {
    changes.clear();
    payloads.clear();
    let mut block = section.read_value_changes()?;
    let head = block.read_head_layout()?;
    let tail = block.read_tail_layout()?;
    let positions = block.read_positions(&head, &tail)?;
    let times = block.read_time_table(&tail)?;

    // The time table is strictly increasing, so the in-range window
    // `[start, end]` maps to a contiguous `time_index` range `[lo, hi)`, found
    // once by binary search. Each change is then filtered by an integer
    // compare on its index, without a per-change lookup into `times`.
    let lo = times.partition_point(|&t| t < start) as u64;
    let hi = times.partition_point(|&t| t <= end) as u64; // exclusive upper bound
    let table_len = times.len() as u64;

    // Collect in-range changes keyed by `time_index` (field 0 holds the index,
    // not the resolved time yet), each PRE-RENDERED into the payload arena as
    // its full VCD line (value + code + '\n'). Each variable's run is already
    // time-ordered; the runs are concatenated here and ordered globally below.
    for (geometry_index, &id) in ids.iter().enumerate() {
        let var = vars.get(id as usize).and_then(Option::as_ref);
        // When filtering, a discarded variable is skipped outright: its waves
        // are not even decoded, and it records no change, so a time step whose
        // only changes are on discarded variables produces no `#time`. Without
        // a filter, a variable with no entry still records zero-length changes
        // so the writer sees their `#time`, matching the unfiltered reference.
        if var.is_none() && filter_active {
            continue;
        }
        let waves = block.read_waves(&head, &positions, id, lengths[geometry_index])?;
        for change in waves.iter() {
            let (time_index, value) = change?;
            if time_index >= table_len {
                return Err(invalid_data("time index out of range"));
            }
            // Pre-`start` changes were already folded into the baseline by the
            // serial prelude; here they are simply skipped.
            if time_index < lo || time_index >= hi {
                continue;
            }
            let rendered_start = payloads.len() as u32;
            if let Some(var) = var {
                render_change(&mut payloads, &value, var, real_endianness);
            }
            let rendered_len = payloads.len() as u32 - rendered_start;
            changes.push((time_index, rendered_start, rendered_len));
        }
    }

    // Stable counting sort by `time_index` (dense, bounded by the time table).
    // The runs are concatenated in variable order, so stability preserves the
    // per-variable order within each time step, matching the reference.
    let n = changes.len();
    let bucket_count = times.len();
    // Per-bucket start offsets via a prefix sum of the counts.
    offsets.clear();
    offsets.resize(bucket_count + 1, 0u32);
    for &(time_index, _, _) in &changes {
        offsets[time_index as usize + 1] += 1;
    }
    for i in 0..bucket_count {
        offsets[i + 1] += offsets[i];
    }
    // Scatter the per-change rendered ranges into time-sorted order in
    // `scratch`, then lay the bytes out contiguously in `framed` in that order
    // and record one `Change` per distinct time (a time-group span), so the
    // writer emits one `#time` and one span per entry. `scratch` holds the
    // sorted `(time, rstart, rlen)` temporarily.
    scratch.clear();
    scratch.resize_with(n, || (0, 0, 0));
    for &(time_index, rstart, rlen) in &changes {
        let slot = &mut offsets[time_index as usize];
        scratch[*slot as usize] = (times[time_index as usize], rstart, rlen);
        *slot += 1;
    }
    framed.clear();
    changes.clear();
    let mut group_time = u64::MAX;
    let mut group_start = 0u32;
    for &(time, rstart, rlen) in scratch.iter() {
        if time != group_time {
            // Close the previous group and open a new one at this time.
            if group_time != u64::MAX {
                changes.push((group_time, group_start, framed.len() as u32 - group_start));
            }
            group_time = time;
            group_start = framed.len() as u32;
        }
        framed.extend_from_slice(&payloads[rstart as usize..(rstart + rlen) as usize]);
    }
    if group_time != u64::MAX {
        changes.push((group_time, group_start, framed.len() as u32 - group_start));
    }

    // Recover the section's backing bytes so the writer can recycle them.
    let bytes = block.data.into_inner();
    Ok(Decoded {
        seq,
        changes,
        bytes,
        scratch,
        payloads,
        offsets,
        framed,
    })
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

/// One TIME-GROUP span, time-sorted within a block: `(time, start, len)` where
/// `[start, start+len)` is the concatenated, pre-rendered VCD output of every
/// change at `time`, laid out contiguously in the block's `framed` buffer.
///
/// During the counting sort the first field transiently holds the `time_index`
/// and the span fields hold the per-change `payloads` range.
type Change = (u64, u32, u32);

/// The reusable buffers that circulate with one in-flight block. Bundling them
/// means one pool slot bounds in-flight blocks and recycles every allocation.
#[derive(Default)]
struct Buffers {
    /// Backing bytes for the buffered section handed to a worker.
    bytes: Vec<u8>,
    /// The decoded, time-sorted change list handed to the writer.
    changes: Vec<Change>,
    /// Scratch scatter target for the counting sort.
    scratch: Vec<Change>,
    /// Per-block arena holding the concatenated bytes of every rendered change;
    /// each change records a `[start, len)` range into it.
    payloads: Vec<u8>,
    /// Counting-sort bucket offsets (one per time-table entry + 1).
    offsets: Vec<u32>,
    /// The block's rendered change bytes laid out contiguously in time-sorted
    /// order; the writer copies time-group spans straight out of here.
    /// `changes` indexes into this, not `payloads`.
    framed: Vec<u8>,
}

/// A bounded pool of reusable [`Buffers`], built on the standard library only
/// (fst-tool takes no threading dependency).
///
/// It doubles as the pipeline's backpressure: the pool is seeded with a fixed
/// number of bundles, the dispatcher `take`s one before reading a block (an
/// empty pool blocks it), and the writer `give`s it back after emitting the
/// block (which wakes the dispatcher). So the number of blocks in flight can
/// never exceed the pool size, and no separate semaphore is needed.
///
/// `shutdown` exists for the error path: if the writer stops early, the
/// dispatcher could otherwise block forever in `take` (no bundle is ever
/// returned again), which would hang the scope that joins every thread.
/// Shutting down makes every current and future `take` return `None`.
struct BufferPool {
    state: std::sync::Mutex<PoolState>,
    available: std::sync::Condvar,
}

struct PoolState {
    free: Vec<Buffers>,
    shutdown: bool,
}

impl BufferPool {
    /// Creates a pool seeded with `size` empty bundles. `size` is the maximum
    /// number of blocks that can be in flight at once.
    fn new(size: usize) -> Self {
        let mut free = Vec::with_capacity(size);
        free.resize_with(size, Buffers::default);
        BufferPool {
            state: std::sync::Mutex::new(PoolState {
                free,
                shutdown: false,
            }),
            available: std::sync::Condvar::new(),
        }
    }

    /// Blocks for a free bundle. Returns `None` if the pool was shut down (in
    /// which case nothing was taken).
    fn take(&self) -> Option<Buffers> {
        let mut state = self.state.lock().unwrap();
        while state.free.is_empty() && !state.shutdown {
            state = self.available.wait(state).unwrap();
        }
        if state.shutdown {
            return None;
        }
        state.free.pop()
    }

    /// Returns a bundle to the pool and wakes a waiter. The buffers keep their
    /// capacity for reuse; they are cleared on next fill.
    fn give(&self, buffers: Buffers) {
        let mut state = self.state.lock().unwrap();
        state.free.push(buffers);
        self.available.notify_one();
    }

    /// Wakes every waiter and makes all future `take` calls return `None`.
    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.shutdown = true;
        self.available.notify_all();
    }
}

/// Emits one block's already-sorted TIME-GROUP spans, carrying the `$dumpvars`
/// state machine and the last-written `#time` across block boundaries so the
/// whole stream reads as one time-ordered dump.
///
/// Each entry is one distinct time whose changes were already rendered AND
/// concatenated in sorted order into `framed` by the parallel decoder (see
/// `decode_block_changes`); the writer only emits the `#time`/`$dumpvars`
/// framing and copies one contiguous span per time. The block is composed into
/// `line` and flushed in one `write_all`. A zero-length span (a time whose
/// changes are all on undeclared variables) still advances `#time`.
fn emit_block_changes<W: Write>(
    out: &mut W,
    changes: &[Change],
    framed: &[u8],
    line: &mut Vec<u8>,
    dumpvars_state: &mut u8,
    current_time: &mut Option<u64>,
) -> std::io::Result<()> {
    line.clear();
    for &(time, start, len) in changes {
        if *current_time != Some(time) {
            // The reference opens `$dumpvars` at the first dumped time and
            // closes it at the second; on the frame path the state starts
            // closed so neither happens here.
            if *dumpvars_state == 1 {
                line.extend_from_slice(b"$end\n");
                *dumpvars_state = 2;
            }
            line.push(b'#');
            push_u64(line, time);
            line.push(b'\n');
            if *dumpvars_state == 0 {
                line.extend_from_slice(b"$dumpvars\n");
                *dumpvars_state = 1;
            }
            *current_time = Some(time);
        }
        if len != 0 {
            line.extend_from_slice(&framed[start as usize..(start + len) as usize]);
        }
    }
    out.write_all(line)
}

/// Two-digit ASCII pairs "00".."99", for formatting two decimal digits per
/// division.
static DEC_PAIRS: [[u8; 2]; 100] = {
    let mut t = [[0u8; 2]; 100];
    let mut i = 0usize;
    while i < 100 {
        t[i] = [b'0' + (i / 10) as u8, b'0' + (i % 10) as u8];
        i += 1;
    }
    t
};

/// Appends `value`'s base-10 ASCII digits to `buf`, two digits per iteration
/// via [`DEC_PAIRS`]. The largest `u64` is 20 digits, so a 20-byte scratch
/// buffer always suffices.
fn push_u64(buf: &mut Vec<u8>, mut value: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while value >= 100 {
        let pair = DEC_PAIRS[(value % 100) as usize];
        value /= 100;
        i -= 2;
        tmp[i] = pair[0];
        tmp[i + 1] = pair[1];
    }
    // Final 1 or 2 digits.
    if value >= 10 {
        let pair = DEC_PAIRS[value as usize];
        i -= 2;
        tmp[i] = pair[0];
        tmp[i + 1] = pair[1];
    } else {
        i -= 1;
        tmp[i] = b'0' + value as u8;
    }
    buf.extend_from_slice(&tmp[i..]);
}

/// Maps each byte to its 8 bits as ASCII `'0'`/`'1'`, most-significant bit
/// first, to expand packed multi-bit values a whole byte at a time.
static BIT_EXPAND: [[u8; 8]; 256] = {
    let mut table = [[0u8; 8]; 256];
    let mut byte = 0usize;
    while byte < 256 {
        let mut bit = 0usize;
        while bit < 8 {
            table[byte][bit] = b'0' | ((byte >> (7 - bit)) & 1) as u8;
            bit += 1;
        }
        byte += 1;
    }
    table
};

/// Renders one value change as its complete VCD line — value, identifier
/// `code`, and trailing `\n` — appended to the payload arena `out`.
fn render_change(out: &mut Vec<u8>, value: &Value, var: &VcdVar, real_endianness: RealEndianess) {
    match value {
        Value::Bit(bit) => {
            out.push(*bit);
        }
        Value::Real(bytes) => {
            out.push(b'r');
            out.extend_from_slice(format_real(*bytes, real_endianness).as_bytes());
            out.push(b' ');
        }
        Value::VarLen(data) => {
            out.push(b's');
            out.extend_from_slice(data);
            out.push(b' ');
        }
        Value::Packed { data, bits } => {
            out.push(if var.is_port { b'p' } else { b'b' });
            // Expand packed bits (MSB-first) to one ASCII '0'/'1' per bit; the
            // table turns each whole input byte into its 8 ASCII chars.
            let bits = *bits;
            let full = bits / 8;
            out.reserve(bits + 1);
            for &byte in &data[..full] {
                out.extend_from_slice(&BIT_EXPAND[byte as usize]);
            }
            // Trailing partial byte: emit only its top `rem` bits.
            let rem = bits % 8;
            if rem != 0 {
                out.extend_from_slice(&BIT_EXPAND[data[full] as usize][..rem]);
            }
            out.push(b' ');
        }
        Value::Ascii(data) => {
            out.push(if var.is_port { b'p' } else { b'b' });
            out.extend_from_slice(data);
            out.push(b' ');
        }
    }
    out.extend_from_slice(var.code.as_bytes());
    out.push(b'\n');
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
    vars: &[Option<VcdVar>],
    baseline: &std::collections::HashMap<u64, Vec<u8>>,
    baseline_real: &std::collections::HashMap<u64, [u8; 8]>,
    real_endianness: RealEndianess,
) -> std::io::Result<()> {
    for &id in ids {
        let Some(Some(var)) = vars.get(id as usize) else {
            continue;
        };
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

fn dump(path: &Path, allow_compressed: bool) -> std::io::Result<()> {
    let input = FstInput::open_path(path)?;
    if input.is_wrapped() && !allow_compressed {
        eprintln!(
            "warning: input is a gzip-wrapped FST; showing only the wrapper section. \
             Pass --allow-compressed to decompress and dump the inner FST."
        );
        let mut iterator = SectionIterator::new(BufReader::new(std::fs::File::open(path)?));
        while let Some(section) = iterator.next()? {
            println!("kind={} len={}", section.block_type, section.len());
        }
        return Ok(());
    }

    let mut reader = input.open()?;

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
    fn glob_match_handles_stars_and_questions() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"", b""));
        assert!(!glob_match(b"", b"x"));
        assert!(glob_match(b"a.b.c", b"a.b.c"));
        assert!(!glob_match(b"a.b.c", b"a.b.d"));
        assert!(glob_match(b"a.*.c", b"a.middle.c"));
        assert!(glob_match(b"a.*", b"a.b.c.d"));
        assert!(glob_match(b"a.b?d", b"a.bxd"));
        assert!(!glob_match(b"a.b?d", b"a.bd"));
        assert!(glob_match(b"*.clk", b"top.cpu.clk"));
        assert!(!glob_match(b"*.clk", b"top.cpu.clock"));
        // A `*` matches an empty run, and adjacent stars collapse.
        assert!(glob_match(b"a**c", b"ac"));
    }

    #[test]
    fn signal_filter_empty_selects_everything() {
        let filter = SignalFilter::new(&[]);
        assert!(filter.is_empty());
        assert!(filter.selects(&["top".into(), "cpu".into()], "clk"));
    }

    #[test]
    fn signal_filter_matches_full_path() {
        let filter = SignalFilter::new(&["top.cpu.clk".to_string()]);
        assert!(filter.selects(&["top".into(), "cpu".into()], "clk"));
        assert!(!filter.selects(&["top".into(), "cpu".into()], "rst"));
        assert!(!filter.selects(&["top".into()], "clk"));
    }

    #[test]
    fn signal_filter_scope_prefix_selects_subtree() {
        // A pattern matching a scope's dotted path selects every variable under
        // it, at any depth.
        let filter = SignalFilter::new(&["top.cpu".to_string()]);
        assert!(filter.selects(&["top".into(), "cpu".into()], "clk"));
        assert!(filter.selects(&["top".into(), "cpu".into(), "regs".into()], "pc"));
        assert!(!filter.selects(&["top".into(), "mem".into()], "data"));
    }

    #[test]
    fn signal_filter_glob_scope_prefix() {
        let filter = SignalFilter::new(&["top.*".to_string()]);
        assert!(filter.selects(&["top".into(), "cpu".into()], "clk"));
        // `top.*` matches the `top.cpu` scope prefix, selecting its subtree.
        assert!(filter.selects(&["top".into(), "cpu".into(), "alu".into()], "out"));
    }

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
    fn minimal_vcd_id_orders_alphabet_then_lexically() {
        // Allocation order: lowercase, uppercase, digits, then special.
        assert_eq!(minimal_vcd_id(1), "a");
        assert_eq!(minimal_vcd_id(26), "z");
        assert_eq!(minimal_vcd_id(27), "A");
        assert_eq!(minimal_vcd_id(52), "Z");
        assert_eq!(minimal_vcd_id(53), "0");
        assert_eq!(minimal_vcd_id(62), "9");
        assert_eq!(minimal_vcd_id(63), "!");
        assert_eq!(minimal_vcd_id(94), "~");
        // Bijective base 94, most-significant digit first: the first two-char
        // code is `aa`, and incrementing changes the last digit (`aa`, `ab`),
        // not the first (`aa`, `ba`).
        assert_eq!(minimal_vcd_id(95), "aa");
        assert_eq!(minimal_vcd_id(96), "ab");
    }

    #[test]
    fn minimal_alphabet_covers_every_legal_code_once() {
        let mut seen = std::collections::HashSet::new();
        for &c in &MINIMAL_ALPHABET {
            assert!((b'!'..=b'~').contains(&c), "code {c} outside VCD range");
            assert!(seen.insert(c), "duplicate code {c}");
        }
        assert_eq!(seen.len(), 94);
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

    #[test]
    fn buffer_pool_recycles_allocation() {
        // A bundle returned to the pool comes back out with its capacity
        // intact, so the next fill reuses the same allocation.
        let pool = BufferPool::new(1);
        let mut bundle = pool.take().unwrap();
        bundle.bytes.extend_from_slice(&[0u8; 1024]);
        bundle.changes.reserve(64);
        let bytes_ptr = bundle.bytes.as_ptr();
        let bytes_cap = bundle.bytes.capacity();
        let changes_cap = bundle.changes.capacity();
        pool.give(bundle);

        let reused = pool.take().unwrap();
        assert_eq!(
            reused.bytes.as_ptr(),
            bytes_ptr,
            "byte buffer was reallocated"
        );
        assert_eq!(reused.bytes.capacity(), bytes_cap);
        assert!(reused.changes.capacity() >= changes_cap);
    }

    #[test]
    fn buffer_pool_take_blocks_until_given_then_shutdown_unblocks() {
        use std::sync::Arc;
        use std::time::Duration;

        // An empty pool blocks `take` (this is the dispatcher backpressure);
        // a `give` wakes it. The waiter returns its bundle so the pool is not
        // left empty for the next phase.
        let pool = Arc::new(BufferPool::new(1));
        let only = pool.take().unwrap();
        let p2 = Arc::clone(&pool);
        let waiter = std::thread::spawn(move || match p2.take() {
            Some(bundle) => {
                p2.give(bundle);
                true
            }
            None => false,
        });
        std::thread::sleep(Duration::from_millis(20));
        pool.give(only);
        assert!(
            waiter.join().unwrap(),
            "give should let a blocked take succeed"
        );

        // With the pool empty again, shutdown makes a blocked take return None
        // (this is the error-path escape that prevents a join deadlock).
        let _held = pool.take().unwrap();
        let p3 = Arc::clone(&pool);
        let waiter = std::thread::spawn(move || p3.take().is_none());
        std::thread::sleep(Duration::from_millis(20));
        pool.shutdown();
        assert!(
            waiter.join().unwrap(),
            "shutdown should unblock take as None"
        );
    }
}
