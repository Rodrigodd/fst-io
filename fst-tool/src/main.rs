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

/// Prints the hierarchy of the file, as an indented tree or one variable per
/// line.
///
/// Entries are printed as they are decoded. With `names`, only the names are
/// printed.
fn info_hierarchy(input: &Path, flat: bool, names: bool) -> std::io::Result<()> {
    let mut iterator = SectionIterator::new(BufReader::new(std::fs::File::open(input)?));
    // Width of the id column, from the number of variables in the header.
    let mut id_width = 1;
    while let Some(section) = iterator.next()? {
        match section.block_type() {
            Some(BlockType::Header) => {
                let header = section.read_header()?;
                id_width = header.num_vars.saturating_sub(1).to_string().len();
                continue;
            }
            Some(BlockType::Hierarchy | BlockType::HierarchyLZ4 | BlockType::HierarchyLZ4Duo) => {}
            _ => continue,
        }

        let mut out = BufWriter::new(std::io::stdout().lock());
        // Names of the open scopes.
        let mut scopes: Vec<String> = Vec::new();
        for entry in section.read_hierarchy()?.entries()? {
            match entry? {
                HierarchyEntry::Scope {
                    scope_type, name, ..
                } => {
                    let name = String::from_utf8_lossy(&name).into_owned();
                    if !flat {
                        let indent = scopes.len() * 2;
                        if names {
                            writeln!(out, "{:indent$}{name}", "")?;
                        } else {
                            // The id and direction columns of a variable are
                            // left blank, and the scope type takes the place
                            // of the variable type.
                            let blank = id_width + 1 + 1 + 7;
                            let scope_type = scope_type_name(scope_type)
                                .map_or_else(|| format!("scope={scope_type}"), str::to_string);
                            writeln!(out, "{:indent$}{:blank$} {scope_type:<8} {name}", "", "")?;
                        }
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
                    // In the tree the whole line is indented, not only the name.
                    let (indent, name) = if flat {
                        let path: String = scopes.iter().map(|s| format!("{s}.")).collect();
                        (0, format!("{path}{name}"))
                    } else {
                        (scopes.len() * 2, name.into_owned())
                    };
                    if names {
                        writeln!(out, "{:indent$}{name}", "")?;
                        continue;
                    }

                    let marker = if is_alias { '*' } else { ' ' };
                    let direction = direction_column(direction);
                    let var_type = var_type_name(var_type)
                        .map_or_else(|| format!("type={var_type}"), str::to_string);
                    writeln!(
                        out,
                        "{:indent$}{id:<id_width$}{marker} {direction:<7} {var_type:<8} {name} ({length})",
                        ""
                    )?;
                }
                HierarchyEntry::AttributeBegin { .. } | HierarchyEntry::AttributeEnd => {}
            }
        }
        return out.flush();
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "file has no hierarchy block",
    ))
}

fn direction_column(direction: u8) -> String {
    var_direction_name(direction).map_or_else(|| format!("dir={direction}"), str::to_string)
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
