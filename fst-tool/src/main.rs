use std::io::Seek;

use fst_io::*;

fn main() -> std::io::Result<()> {
    let file = std::env::args().nth(1).expect("Please provide a file path");

    let file = std::fs::File::open(file).expect("Failed to open file");
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
