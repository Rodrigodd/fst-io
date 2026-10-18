use std::io::{BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
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
        Command::Splice {
            input,
            start_time,
            end_time,
            output,
        } => splice(&input, start_time, end_time, &output),
    }
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
