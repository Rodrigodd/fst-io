use super::*;

fn section(block_type: u8, data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![block_type];
    bytes.extend_from_slice(&(data.len() as u64 + 8).to_be_bytes());
    bytes.extend_from_slice(data);
    bytes
}

fn geometry(lengths: &[u8], count: u64) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&(lengths.len() as u64).to_be_bytes());
    data.extend_from_slice(&count.to_be_bytes());
    data.extend_from_slice(lengths);
    section(BlockType::Geometry as u8, &data)
}

#[test]
fn uncompressed_geometry() {
    // 1, 0 (real), 0xFFFFFFFF (zero length), 300
    let lengths = [0x01, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0xAC, 0x02];
    let mut file = geometry(&lengths, 4);
    file.extend(section(BlockType::Blackout as u8, &[1, 2, 3]));

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));

    let geometry = iterator.next().unwrap().unwrap().read_geometry().unwrap();
    assert_eq!(geometry.count, 4);
    assert!(!geometry.is_compressed());
    let lengths: Vec<_> = geometry.lengths().map(Result::unwrap).collect();
    assert_eq!(
        lengths,
        [
            VariableLength::Bits(1),
            VariableLength::Real,
            VariableLength::VarLen,
            VariableLength::Bits(300),
        ]
    );

    let next = iterator.next().unwrap().unwrap();
    assert_eq!(next.block_type(), Some(BlockType::Blackout));
    assert_eq!(next.len(), 3);
}

#[test]
fn compressed_geometry() {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    // 300 entries of 8 bits, 0 (real), 0xFFFFFFFF (zero length)
    let mut lengths = vec![0x08; 300];
    lengths.extend([0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&lengths).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < lengths.len());

    let mut data = Vec::new();
    data.extend_from_slice(&(lengths.len() as u64).to_be_bytes());
    data.extend_from_slice(&302u64.to_be_bytes());
    data.extend_from_slice(&compressed);
    let mut file = section(BlockType::Geometry as u8, &data);
    file.extend(section(BlockType::Blackout as u8, &[]));

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));

    let geometry = iterator.next().unwrap().unwrap().read_geometry().unwrap();
    assert!(geometry.is_compressed());
    let lengths: Vec<_> = geometry.lengths().map(Result::unwrap).collect();
    assert_eq!(lengths.len(), 302);
    assert!(lengths[..300].iter().all(|&l| l == VariableLength::Bits(8)));
    assert_eq!(lengths[300], VariableLength::Real);
    assert_eq!(lengths[301], VariableLength::VarLen);

    let next = iterator.next().unwrap().unwrap();
    assert_eq!(next.block_type(), Some(BlockType::Blackout));
}

#[test]
fn partially_read_geometry_is_skipped() {
    let mut file = geometry(&[0x01, 0x02, 0x03], 3);
    file.extend(section(BlockType::Blackout as u8, &[]));

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));

    let geometry = iterator.next().unwrap().unwrap().read_geometry().unwrap();
    let first = geometry.lengths().next().unwrap().unwrap();
    assert_eq!(first, VariableLength::Bits(1));

    let next = iterator.next().unwrap().unwrap();
    assert_eq!(next.block_type(), Some(BlockType::Blackout));
    assert!(iterator.next().unwrap().is_none());
}

#[test]
fn truncated_varint_is_an_error() {
    let file = geometry(&[0x80], 1);
    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let geometry = iterator.next().unwrap().unwrap().read_geometry().unwrap();
    let mut lengths = geometry.lengths();
    assert!(lengths.next().unwrap().is_err());
    assert!(lengths.next().is_none());
}

/// Encodes `value` as a LEB128 varint.
fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(byte);
            return bytes;
        }
        bytes.push(byte | 0x80);
    }
}

fn lz4(data: &[u8]) -> Vec<u8> {
    let mut output = vec![0; lz4_flex::block::get_maximum_output_size(data.len())];
    let len = lz4_flex::block::compress_into(data, &mut output).unwrap();
    output.truncate(len);
    output
}

fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn var(name: &str, length: u64, alias: u64) -> Vec<u8> {
    let mut bytes = vec![16, 1]; // FST_VT_VCD_WIRE, FST_VD_INPUT
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    bytes.extend(varint(length));
    bytes.extend(varint(alias));
    bytes
}

fn var_entry(name: &str, length: u64, id: u64, is_alias: bool) -> HierarchyEntry {
    HierarchyEntry::Var {
        var_type: 16,
        direction: 1,
        name: name.as_bytes().to_vec(),
        length,
        id,
        is_alias,
    }
}

/// Raw hierarchy data and the entries it decodes to.
fn sample_hierarchy() -> (Vec<u8>, Vec<HierarchyEntry>) {
    let mut data = vec![TAG_ATTRIBUTE_BEGIN, 0, 3];
    data.extend(b"top.v\0");
    data.extend(varint(300));
    data.push(TAG_SCOPE);
    data.push(0);
    data.extend(b"top\0\0");
    data.push(TAG_ATTRIBUTE_BEGIN);
    data.extend([2, 0]);
    data.extend(b"enum\0");
    data.extend(varint(1));
    data.push(TAG_ATTRIBUTE_END);
    data.extend(var("a", 1, 0));
    data.extend(var("b", 8, 0));
    data.extend(var("c", 8, 2)); // alias to b
    data.push(TAG_UPSCOPE);

    let entries = vec![
        HierarchyEntry::AttributeBegin {
            attr_type: 0,
            subtype: 3,
            name: b"top.v".to_vec(),
            value: 300,
        },
        HierarchyEntry::Scope {
            scope_type: 0,
            name: b"top".to_vec(),
            component: b"".to_vec(),
        },
        HierarchyEntry::AttributeBegin {
            attr_type: 2,
            subtype: 0,
            name: b"enum".to_vec(),
            value: 1,
        },
        HierarchyEntry::AttributeEnd,
        var_entry("a", 1, 0, false),
        var_entry("b", 8, 1, false),
        var_entry("c", 8, 1, true),
        HierarchyEntry::UpScope,
    ];
    (data, entries)
}

/// Builds a hierarchy section with the given block type and compressed data.
fn hierarchy_section(block_type: BlockType, uncompressed_length: usize, data: &[u8]) -> Vec<u8> {
    let mut bytes = (uncompressed_length as u64).to_be_bytes().to_vec();
    bytes.extend_from_slice(data);
    let mut file = section(block_type as u8, &bytes);
    file.extend(section(BlockType::Blackout as u8, &[]));
    file
}

fn read_hierarchy_entries(file: Vec<u8>, compression: HierarchyCompression) -> Vec<HierarchyEntry> {
    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let hierarchy = iterator.next().unwrap().unwrap().read_hierarchy().unwrap();
    assert_eq!(hierarchy.compression, compression);
    let entries = hierarchy.entries().unwrap().map(Result::unwrap).collect();

    let next = iterator.next().unwrap().unwrap();
    assert_eq!(next.block_type(), Some(BlockType::Blackout));
    entries
}

#[test]
fn gzip_hierarchy() {
    let (data, expected) = sample_hierarchy();
    let file = hierarchy_section(BlockType::Hierarchy, data.len(), &gzip(&data));
    let entries = read_hierarchy_entries(file, HierarchyCompression::Gzip);
    assert_eq!(entries, expected);
}

#[test]
fn lz4_hierarchy() {
    let (data, expected) = sample_hierarchy();
    let file = hierarchy_section(BlockType::HierarchyLZ4, data.len(), &lz4(&data));
    let entries = read_hierarchy_entries(file, HierarchyCompression::Lz4);
    assert_eq!(entries, expected);
}

#[test]
fn lz4_duo_hierarchy() {
    let (data, expected) = sample_hierarchy();
    let once = lz4(&data);
    let mut compressed = varint(once.len() as u64);
    compressed.extend(lz4(&once));
    let file = hierarchy_section(BlockType::HierarchyLZ4Duo, data.len(), &compressed);
    let entries = read_hierarchy_entries(
        file,
        HierarchyCompression::Lz4Duo {
            compressed_once_length: once.len() as u64,
        },
    );
    assert_eq!(entries, expected);
}

#[test]
fn hierarchy_alias_ids() {
    // The example from the spec.
    let aliases = [0, 0, 0, 0, 2, 1, 0, 0, 6, 0];
    let expected = [
        (0, false),
        (1, false),
        (2, false),
        (3, false),
        (1, true),
        (0, true),
        (4, false),
        (5, false),
        (5, true),
        (6, false),
    ];

    let data: Vec<u8> = aliases
        .iter()
        .flat_map(|&alias| var("v", 1, alias))
        .collect();
    let file = hierarchy_section(BlockType::HierarchyLZ4, data.len(), &lz4(&data));
    let entries = read_hierarchy_entries(file, HierarchyCompression::Lz4);

    let ids: Vec<_> = entries
        .iter()
        .map(|entry| match entry {
            HierarchyEntry::Var { id, is_alias, .. } => (*id, *is_alias),
            _ => panic!("expected a var"),
        })
        .collect();
    assert_eq!(ids, expected);
}

#[test]
fn truncated_hierarchy_is_an_error() {
    let (mut data, _) = sample_hierarchy();
    data.truncate(data.len() - 4); // cut inside the last var
    let file = hierarchy_section(BlockType::Hierarchy, data.len(), &gzip(&data));

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let hierarchy = iterator.next().unwrap().unwrap().read_hierarchy().unwrap();
    let entries: Vec<_> = hierarchy.entries().unwrap().collect();
    assert!(entries.last().unwrap().is_err());
    assert!(entries[..entries.len() - 1].iter().all(Result::is_ok));
}

#[test]
fn bad_lz4_hierarchy_length_is_an_error() {
    let (data, _) = sample_hierarchy();
    let file = hierarchy_section(BlockType::HierarchyLZ4, data.len() + 1, &lz4(&data));

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let hierarchy = iterator.next().unwrap().unwrap().read_hierarchy().unwrap();
    assert!(hierarchy.entries().is_err());
}

/// Encodes `value` as a signed LEB128 varint.
fn svarint(mut value: i64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if done {
            bytes.push(byte);
            return bytes;
        }
        bytes.push(byte | 0x80);
    }
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn svarint_roundtrip() {
    for value in [0, 1, -1, 63, 64, -64, -65, 201, -5, i64::MAX, i64::MIN] {
        let bytes = svarint(value);
        assert_eq!(read_svarint(&mut &bytes[..]).unwrap(), value, "{value}");
    }
}

#[test]
// `0 << 1 | 1` keeps the `value << shift | flag` layout aligned with the other
// encoded fields.
#[allow(clippy::identity_op)]
fn positions_alias2_spec_example() {
    // 0 0 100 0 -3 0 200 350 -3
    let mut data = varint(2 << 1);
    data.extend(svarint(100 << 1 | 1));
    data.extend(varint(1 << 1));
    data.extend(svarint(-3 << 1 | 1));
    data.extend(varint(1 << 1));
    data.extend(svarint(100 << 1 | 1));
    data.extend(svarint(150 << 1 | 1));
    data.extend(svarint(0 << 1 | 1));

    let positions = decode_positions_alias2(&data, 10, 400).unwrap();
    assert_eq!(
        positions,
        [
            WavePosition::None,
            WavePosition::None,
            WavePosition::Data {
                offset: 100,
                length: 100
            },
            WavePosition::None,
            WavePosition::Alias(2),
            WavePosition::None,
            WavePosition::Data {
                offset: 200,
                length: 150
            },
            WavePosition::Data {
                offset: 350,
                length: 50
            },
            WavePosition::Alias(2),
            // Trailing entry left out of the table.
            WavePosition::None,
        ]
    );
}

#[test]
fn positions_dyn_alias() {
    // Same as the spec example, in the FST_BL_VCDATA_DYN_ALIAS encoding.
    let mut data = varint(2 << 1);
    data.extend(varint(100 << 1 | 1));
    data.extend(varint(1 << 1));
    data.extend(varint(0));
    data.extend(varint(3));
    data.extend(varint(1 << 1));
    data.extend(varint(100 << 1 | 1));
    data.extend(varint(150 << 1 | 1));
    data.extend(varint(0));
    data.extend(varint(3));

    let positions = decode_positions(&data, 9, 400).unwrap();
    assert_eq!(
        positions[2],
        WavePosition::Data {
            offset: 100,
            length: 100
        }
    );
    assert_eq!(positions[4], WavePosition::Alias(2));
    assert_eq!(
        positions[7],
        WavePosition::Data {
            offset: 350,
            length: 50
        }
    );
    assert_eq!(positions[8], WavePosition::Alias(2));
}

#[test]
fn positions_errors() {
    // More entries than variables.
    assert!(decode_positions(&varint(3 << 1), 2, 10).is_err());
    // Offset past the end of the waves.
    assert!(decode_positions(&varint(20 << 1 | 1), 2, 10).is_err());
    // Repeated alias without a previous one.
    assert!(decode_positions_alias2(&svarint(1), 2, 10).is_err());
}

fn waves(data: Vec<u8>, length: VariableLength) -> Vec<(u64, Vec<u8>)> {
    let waves = Waves { data, length };
    waves
        .iter()
        .map(|change| {
            let (time_index, value) = change.unwrap();
            (time_index, value.to_ascii())
        })
        .collect()
}

#[test]
// `0 << 2` keeps the `delta << 2 | kind << 1 | bit` layout explicit.
#[allow(clippy::identity_op)]
fn one_bit_waves() {
    let mut data = Vec::new();
    data.extend(varint(0 << 2 | 1 << 1)); // 1 at 0
    data.extend(varint(1 << 2)); // 0 at 1
    for (i, _) in ONE_BIT_VALUES.iter().enumerate() {
        data.extend(varint(2 << 4 | (i as u64) << 1 | 1));
    }
    let changes = waves(data, VariableLength::Bits(1));

    let mut expected = vec![(0, b"1".to_vec()), (1, b"0".to_vec())];
    for (i, &value) in ONE_BIT_VALUES.iter().enumerate() {
        expected.push((3 + 2 * i as u64, vec![value]));
    }
    assert_eq!(changes, expected);
}

#[test]
fn multi_bit_waves() {
    let mut data = varint(0);
    data.extend([0b1011_0011, 0b1000_0000]); // 9 bits packed
    data.extend(varint(3 << 1 | 1));
    data.extend(b"01xz0101z");
    let changes = waves(data, VariableLength::Bits(9));
    assert_eq!(
        changes,
        [(0, b"101100111".to_vec()), (3, b"01xz0101z".to_vec())]
    );
}

#[test]
fn real_and_varlen_waves() {
    let mut data = varint(1 << 1 | 1);
    data.extend(1.5f64.to_le_bytes());
    data.extend(varint(1 << 1));
    data.push(0b0110_0101); // every byte is b'0' or b'1'
    let waves = Waves {
        data,
        length: VariableLength::Real,
    };
    let changes: Vec<_> = waves.iter().map(|change| change.unwrap()).collect();
    assert_eq!(changes[0], (1, Value::Real(1.5f64.to_le_bytes())));
    assert_eq!(changes[1], (2, Value::Real(*b"01100101")));

    let mut data = varint(0);
    data.extend(varint(5));
    data.extend(b"hello");
    data.extend(varint(4 << 1));
    data.extend(varint(0));
    let waves = Waves {
        data,
        length: VariableLength::VarLen,
    };
    let changes: Vec<_> = waves.iter().map(|change| change.unwrap()).collect();
    assert_eq!(
        changes,
        [(0, Value::VarLen(b"hello")), (4, Value::VarLen(b""))]
    );
}

#[test]
fn truncated_waves_is_an_error() {
    let mut data = varint(0);
    data.push(0xFF); // needs 2 bytes
    let waves = Waves {
        data,
        length: VariableLength::Bits(9),
    };
    let changes: Vec<_> = waves.iter().collect();
    assert_eq!(changes.len(), 1);
    assert!(changes[0].is_err());
}

/// Builds a FST_BL_VCDATA_DYN_ALIAS block with 4 variables:
/// 0: 1 bit, uncompressed waves
/// 1: 4 bits, zlib compressed waves
/// 2: 4 bits, dynamic alias of 1
/// 3: 1 bit, no changes
fn value_change_block() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend(10u64.to_be_bytes()); // start time
    data.extend(22u64.to_be_bytes()); // end time
    data.extend(64u64.to_be_bytes()); // memory required

    let bits = b"0xxxx1010z";
    let compressed_bits = zlib(bits);
    data.extend(varint(bits.len() as u64));
    data.extend(varint(compressed_bits.len() as u64));
    data.extend(varint(4));
    data.extend(&compressed_bits);

    data.extend(varint(4)); // waves count
    data.push(b'Z');

    let mut waves0 = varint(0);
    waves0.extend(varint(1 << 1)); // 1 at time index 0
    waves0.extend(varint(1 << 4 | 1 << 1 | 1)); // z at time index 1

    let mut changes1 = varint(0); // packed 1010 at time index 0
    changes1.push(0b1010_0000);
    changes1.extend(varint(2 << 1 | 1)); // x01z at time index 2
    changes1.extend(b"x01z");
    let mut waves1 = varint(changes1.len() as u64);
    waves1.extend(zlib(&changes1));

    data.extend(&waves0);
    data.extend(&waves1);

    let mut positions = varint(1 << 1 | 1);
    positions.extend(varint((waves0.len() as u64) << 1 | 1));
    positions.extend(varint(0));
    positions.extend(varint(2));
    positions.extend(varint(1 << 1));
    data.extend(&positions);
    data.extend((positions.len() as u64).to_be_bytes());

    let mut times = varint(10);
    times.extend(varint(5));
    times.extend(varint(7));
    data.extend(&times);
    data.extend((times.len() as u64).to_be_bytes());
    data.extend((times.len() as u64).to_be_bytes());
    data.extend(3u64.to_be_bytes());

    let mut file = section(BlockType::ValueChangeDataDynamicAlias as u8, &data);
    file.extend(section(BlockType::Blackout as u8, &[]));
    file
}

#[test]
fn value_change_block_end_to_end() {
    let lengths = [
        VariableLength::Bits(1),
        VariableLength::Bits(4),
        VariableLength::Bits(4),
        VariableLength::Bits(1),
    ];
    let mut iterator = SectionIterator::new(std::io::Cursor::new(value_change_block()));
    let mut block = iterator
        .next()
        .unwrap()
        .unwrap()
        .read_value_changes()
        .unwrap();

    assert_eq!(block.start_time, 10);
    assert_eq!(block.end_time, 22);
    assert_eq!(block.memory_required, 64);

    // Read the tables out of order, to exercise seeking.
    let tail = block.read_tail_layout().unwrap();
    let head = block.read_head_layout().unwrap();
    assert_eq!(head.waves_count, 4);
    assert_eq!(head.pack_type, b'Z');
    assert_eq!(tail.time_count, 3);
    assert_eq!(block.read_time_table(&tail).unwrap(), [10, 15, 22]);

    let positions = block.read_positions(&head, &tail).unwrap();
    let changes = |block: &mut ValueChangeBlock<_>, id: u64| -> Vec<(u64, Vec<u8>)> {
        let waves = block
            .read_waves(&head, &positions, id, lengths[id as usize])
            .unwrap();
        waves
            .iter()
            .map(|change| {
                let (time_index, value) = change.unwrap();
                (time_index, value.to_ascii())
            })
            .collect()
    };
    let expected1 = vec![(0, b"1010".to_vec()), (2, b"x01z".to_vec())];
    assert_eq!(changes(&mut block, 1), expected1);
    assert_eq!(
        changes(&mut block, 0),
        [(0, b"1".to_vec()), (1, b"z".to_vec())]
    );
    assert_eq!(changes(&mut block, 2), expected1);
    assert_eq!(changes(&mut block, 3), []);

    let bits = block.read_bits(&head).unwrap();
    let values: Vec<_> = bits
        .values(&lengths)
        .map(|v| v.unwrap().to_ascii())
        .collect();
    assert_eq!(
        values,
        [
            b"0".to_vec(),
            b"xxxx".to_vec(),
            b"1010".to_vec(),
            b"z".to_vec()
        ]
    );

    assert_eq!(positions[2], WavePosition::Alias(1));

    let next = iterator.next().unwrap().unwrap();
    assert_eq!(next.block_type(), Some(BlockType::Blackout));
}

#[test]
fn fastlz_waves() {
    let mut changes = Vec::new();
    for i in 0..100u64 {
        changes.extend(varint(1 << 2 | (i & 1) << 1));
    }
    for level in [
        fastlz_rs::CompressionLevel::Level1,
        fastlz_rs::CompressionLevel::Level2,
    ] {
        let compressed = fastlz_rs::CompressState::new()
            .compress_to_vec(&changes, level)
            .unwrap();
        assert!(compressed.len() < changes.len());
        let decompressed = fastlz_decompress(&compressed, changes.len() as u64).unwrap();
        assert_eq!(decompressed, changes);
        assert!(fastlz_decompress(&compressed, changes.len() as u64 + 1).is_err());
    }
}

#[test]
fn value_change_block_corrupt_trailer() {
    let mut file = value_change_block();
    // The blackout section is 9 bytes, the time count is the last field.
    let trailer = file.len() - 9 - 24;
    // Time table compressed length larger than the block.
    file[trailer + 8..trailer + 16].copy_from_slice(&u64::MAX.to_be_bytes());
    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let mut block = iterator
        .next()
        .unwrap()
        .unwrap()
        .read_value_changes()
        .unwrap();
    assert!(block.read_tail_layout().is_err());
}

#[test]
fn header_roundtrip() {
    let mut bytes = [0u8; HeaderBlock::LEN];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = i as u8;
    }
    bytes[16..24].copy_from_slice(&std::f64::consts::E.to_le_bytes());
    let file = section(BlockType::Header as u8, &bytes);

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let header = iterator.next().unwrap().unwrap().read_header().unwrap();
    assert_eq!(header.to_bytes(), bytes);
}

#[test]
fn copy_sections() {
    let mut file = geometry(&[0x01, 0x02], 2);
    file.extend(value_change_block());
    let mut iterator = SectionIterator::new(std::io::Cursor::new(file.clone()));

    let mut copy = Vec::new();
    iterator
        .next()
        .unwrap()
        .unwrap()
        .copy_to(&mut copy)
        .unwrap();
    let mut block = iterator
        .next()
        .unwrap()
        .unwrap()
        .read_value_changes()
        .unwrap();
    // The block is copied whole even after its fixed fields were read.
    block.copy_to(&mut copy).unwrap();
    assert!(iterator.next().unwrap().is_some()); // blackout, not copied
    assert_eq!(copy, file[..copy.len()]);
}

/// Wraps `inner` as a whole-file gzip wrapper, claiming `uncompressed_len`.
///
/// The claimed length is what [`FstInput`] reads to choose memory vs temp
/// storage; a test can overstate it to force the temp-file path.
fn gzip_wrapper(inner: &[u8], uncompressed_len: u64) -> Vec<u8> {
    let compressed = gzip(inner);
    let mut body = Vec::new();
    body.extend_from_slice(&uncompressed_len.to_be_bytes());
    body.extend_from_slice(&compressed);
    section(BlockType::GzippedWrapper as u8, &body)
}

fn write_temp(bytes: &[u8], tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("fst-io-test-{}-{}.fst", std::process::id(), tag));
    std::fs::write(&path, bytes).unwrap();
    path
}

fn sample_inner_file() -> Vec<u8> {
    let lengths = [0x01, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0xAC, 0x02];
    let mut file = geometry(&lengths, 4);
    file.extend(section(BlockType::Blackout as u8, &[1, 2, 3]));
    file
}

#[test]
fn unwrapped_input_is_read_in_place() {
    let inner = sample_inner_file();
    let path = write_temp(&inner, "plain");
    let input = FstInput::open_path(&path).unwrap();
    assert!(!input.is_wrapped());
    let mut read = Vec::new();
    input.open().unwrap().read_to_end(&mut read).unwrap();
    assert_eq!(read, inner);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn gzip_wrapper_decompresses_to_the_inner_file() {
    let inner = sample_inner_file();
    let path = write_temp(&gzip_wrapper(&inner, inner.len() as u64), "memory");
    let input = FstInput::open_path(&path).unwrap();
    assert!(input.is_wrapped());
    let mut read = Vec::new();
    input.open().unwrap().read_to_end(&mut read).unwrap();
    assert_eq!(read, inner);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn large_gzip_wrapper_uses_a_temp_file() {
    let inner = sample_inner_file();
    let claimed = WRAPPER_MEMORY_LIMIT + 1;
    let path = write_temp(&gzip_wrapper(&inner, claimed), "temp");
    let input = FstInput::open_path(&path).unwrap();
    assert!(matches!(input, FstInput::Temp(_)));
    let mut read = Vec::new();
    input.open().unwrap().read_to_end(&mut read).unwrap();
    assert_eq!(read, inner);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn wrapped_and_plain_iterate_the_same_sections() {
    let inner = sample_inner_file();
    let plain = write_temp(&inner, "eq-plain");
    let wrapped = write_temp(&gzip_wrapper(&inner, inner.len() as u64), "eq-wrapped");

    let kinds = |path: &Path| -> Vec<u8> {
        let mut iterator = SectionIterator::new(FstInput::open_path(path).unwrap().open().unwrap());
        let mut kinds = Vec::new();
        while let Some(section) = iterator.next().unwrap() {
            kinds.push(section.block_type);
        }
        kinds
    };

    assert_eq!(kinds(&plain), kinds(&wrapped));
    std::fs::remove_file(&plain).unwrap();
    std::fs::remove_file(&wrapped).unwrap();
}

#[test]
fn temp_file_is_removed_on_drop() {
    let inner = sample_inner_file();
    let path = write_temp(&gzip_wrapper(&inner, WRAPPER_MEMORY_LIMIT + 1), "drop");
    let input = FstInput::open_path(&path).unwrap();
    let FstInput::Temp(ref temp) = input else {
        panic!("expected temp-file input");
    };
    let temp_path = temp.path().to_path_buf();
    assert!(temp_path.exists());
    drop(input);
    assert!(!temp_path.exists());
    std::fs::remove_file(&path).unwrap();
}

/// Builds a var record with an explicit var type, for geometry derivation.
fn typed_var(var_type: u8, name: &str, length: u64, alias: u64) -> Vec<u8> {
    let mut bytes = vec![var_type, 1];
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    bytes.extend(varint(length));
    bytes.extend(varint(alias));
    bytes
}

#[test]
fn section_iterator_stops_at_skip_block() {
    // A real section, then a Skip tag whose length field is zero: the writer
    // leaves the section it is still building tagged Skip at the tail of an
    // interrupted dump.
    let mut file = section(BlockType::Blackout as u8, &[1, 2, 3]);
    file.push(BlockType::Skip as u8);
    file.extend_from_slice(&0u64.to_be_bytes());
    file.extend_from_slice(&[0xDE, 0xAD]); // trailing garbage past the tag

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));

    let first = iterator.next().unwrap().unwrap();
    assert_eq!(first.block_type(), Some(BlockType::Blackout));
    assert_eq!(first.len(), 3);

    assert!(iterator.next().unwrap().is_none());
}

#[test]
fn skip_tag_with_zero_length_does_not_error() {
    // A file that is only a Skip tag with a zero length field is an empty,
    // interrupted dump; iteration ends rather than erroring on the length.
    let mut file = vec![BlockType::Skip as u8];
    file.extend_from_slice(&0u64.to_be_bytes());

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    assert!(iterator.next().unwrap().is_none());
}

#[test]
fn from_records_parses_uncompressed_hierarchy() {
    // The sibling `.hier` file holds the same records as a hierarchy section
    // body, uncompressed and without any header or length prefix.
    let (data, expected) = sample_hierarchy();
    let entries: Vec<_> = HierarchyEntries::from_records(std::io::Cursor::new(data))
        .map(Result::unwrap)
        .collect();
    assert_eq!(entries, expected);
}

#[test]
fn geometry_from_entries_matches_geometry_block() {
    // One of each geometry case: a multi-bit wire, a real, a zero-length
    // string, a wire alias (no geometry entry), and another wire.
    let mut data = Vec::new();
    data.extend(typed_var(16, "wide", 8, 0)); // FST_VT_VCD_WIRE
    data.extend(typed_var(3, "r", 8, 0)); // FST_VT_VCD_REAL (length is 8 in the record)
    data.extend(typed_var(21, "s", 0, 0)); // FST_VT_GEN_STRING, zero length
    data.extend(typed_var(16, "a", 8, 2)); // alias of the wide wire
    data.extend(typed_var(16, "scalar", 1, 0));

    let entries: Vec<_> = HierarchyEntries::from_records(std::io::Cursor::new(data)).collect();
    let lengths = geometry_from_entries(entries).unwrap();

    assert_eq!(
        lengths,
        [
            VariableLength::Bits(8),
            VariableLength::Real,
            VariableLength::VarLen,
            VariableLength::Bits(1),
        ]
    );
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
fn format_g_trims_trailing_zeros_and_matches_c() {
    assert_eq!(format_g(0.0, 16), "0");
    assert_eq!(format_g(1.25, 16), "1.25");
    assert_eq!(format_g(-3.5, 16), "-3.5");
    assert_eq!(format_g(100.0, 16), "100");
    // A value whose exponent reaches the scientific-notation threshold.
    assert_eq!(format_g(1e20, 16), "1e20");
}

/// A 321-byte header section with the given time range; other fields are zero
/// except the real-endianness marker, which `read_header` validates.
fn header_section(start_time: u64, end_time: u64) -> Vec<u8> {
    let mut bytes = [0u8; HeaderBlock::LEN];
    bytes[0..8].copy_from_slice(&start_time.to_be_bytes());
    bytes[8..16].copy_from_slice(&end_time.to_be_bytes());
    bytes[16..24].copy_from_slice(&std::f64::consts::E.to_le_bytes());
    section(BlockType::Header as u8, &bytes)
}

/// A whole FST file an [`FstReader`] can open: a header, a geometry block for
/// the four variables `value_change_block` describes (lengths 1, 4, 4, 1), and
/// that value-change block.
fn reader_fixture() -> Vec<u8> {
    let mut file = header_section(10, 22);
    file.extend(geometry(&[1, 4, 4, 1], 4));
    file.extend(value_change_block());
    file
}

#[test]
fn read_value_changes_streams_all_changes_with_absolute_time() {
    let path = write_temp(&reader_fixture(), "reader-vc");
    let reader = FstReader::open(FstInput::open_path(&path).unwrap()).unwrap();
    assert_eq!(reader.header().start_time, 10);
    assert_eq!(
        reader.geometry(),
        [
            VariableLength::Bits(1),
            VariableLength::Bits(4),
            VariableLength::Bits(4),
            VariableLength::Bits(1),
        ]
    );

    let mut seen: Vec<(u64, usize, Vec<u8>)> = Vec::new();
    reader
        .read_value_changes(
            TimeFilter {
                start: 0,
                end: u64::MAX,
            },
            None,
            |time, handle, value| {
                seen.push((time, handle.index(), value.to_ascii()));
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .unwrap();

    // Per-signal then per-change: geometry index 0, 1, then 2 (alias of 1); 3
    // has no changes. Time indices 0/1/2 resolve to 10/15/22.
    assert_eq!(
        seen,
        [
            (10, 0, b"1".to_vec()),
            (15, 0, b"z".to_vec()),
            (10, 1, b"1010".to_vec()),
            (22, 1, b"x01z".to_vec()),
            (10, 2, b"1010".to_vec()),
            (22, 2, b"x01z".to_vec()),
        ]
    );
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_value_changes_filters_by_handle() {
    let path = write_temp(&reader_fixture(), "reader-vc-filter");
    let reader = FstReader::open(FstInput::open_path(&path).unwrap()).unwrap();

    let include = [FstSignalHandle::from_index(0)];
    let mut handles: Vec<usize> = Vec::new();
    reader
        .read_value_changes(
            TimeFilter {
                start: 0,
                end: u64::MAX,
            },
            Some(&include),
            |_time, handle, _value| {
                handles.push(handle.index());
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .unwrap();
    assert_eq!(handles, [0, 0]);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_value_changes_honours_the_time_window() {
    let path = write_temp(&reader_fixture(), "reader-vc-window");
    let reader = FstReader::open(FstInput::open_path(&path).unwrap()).unwrap();

    // Window [15, 22] drops the time-index-0 (t=10) changes, keeps 15 and 22.
    let mut times: Vec<u64> = Vec::new();
    reader
        .read_value_changes(TimeFilter { start: 15, end: 22 }, None, |time, _h, _v| {
            times.push(time);
            Ok::<(), std::convert::Infallible>(())
        })
        .unwrap();
    assert_eq!(times, [15, 22, 22]);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_value_changes_propagates_callback_error() {
    let path = write_temp(&reader_fixture(), "reader-vc-err");
    let reader = FstReader::open(FstInput::open_path(&path).unwrap()).unwrap();

    let result = reader.read_value_changes(
        TimeFilter {
            start: 0,
            end: u64::MAX,
        },
        None,
        |_time, _handle, _value| Err("stop"),
    );
    assert!(matches!(result, Err(ReadError::Callback("stop"))));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn write_varint_is_read_back_by_read_varint() {
    for value in [
        0,
        1,
        127,
        128,
        300,
        16_383,
        16_384,
        u64::from(u32::MAX),
        u64::MAX,
    ] {
        let mut bytes = Vec::new();
        write_varint(&mut bytes, value);
        assert_eq!(bytes, varint(value), "{value}");
        let mut reader = &bytes[..];
        assert_eq!(read_varint(&mut reader).unwrap(), value);
        assert!(reader.is_empty());
    }
}

#[test]
fn write_record_reproduces_the_hierarchy_records() {
    let (data, entries) = sample_hierarchy();
    let mut encoded = Vec::new();
    for entry in &entries {
        entry.write_record(&mut encoded);
    }
    assert_eq!(encoded, data);
}

#[test]
fn write_record_ids_follow_record_order() {
    let aliases = [0, 0, 0, 0, 2, 1, 0, 0, 6, 0];
    let data: Vec<u8> = aliases
        .iter()
        .flat_map(|&alias| var("v", 1, alias))
        .collect();
    let entries: Vec<_> = HierarchyEntries::from_records(std::io::Cursor::new(&data[..]))
        .map(Result::unwrap)
        .collect();

    let mut encoded = Vec::new();
    for entry in &entries {
        entry.write_record(&mut encoded);
    }
    assert_eq!(encoded, data);
}

#[test]
fn to_geometry_is_read_back_by_geometry_lengths() {
    let lengths = [
        VariableLength::Bits(1),
        VariableLength::Real,
        VariableLength::VarLen,
        VariableLength::Bits(300),
        VariableLength::Bits(0xFFFF_FFFE),
    ];
    let mut data = Vec::new();
    for length in lengths {
        write_varint(&mut data, length.to_geometry());
    }
    let file = geometry(&data, lengths.len() as u64);

    let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
    let geometry = iterator.next().unwrap().unwrap().read_geometry().unwrap();
    let read: Vec<_> = geometry.lengths().map(Result::unwrap).collect();
    assert_eq!(read, lengths);
}

/// A value-change section decoded with the block readers.
struct DecodedBlock {
    block_type: BlockType,
    start_time: u64,
    end_time: u64,
    memory_required: u64,
    times: Vec<u64>,
    /// The frame value of each signal, as ASCII (raw bytes for reals).
    frame: Vec<Vec<u8>>,
    positions: Vec<WavePosition>,
    /// Each signal's changes, as (time index, ASCII).
    changes: Vec<Vec<(u64, Vec<u8>)>>,
    tail: TailLayout,
    head: HeadLayout,
}

fn decode_block(section: &[u8], geometry: &[VariableLength]) -> DecodedBlock {
    let mut iterator = SectionIterator::new(std::io::Cursor::new(section.to_vec()));
    let section = iterator.next().unwrap().unwrap().buffer().unwrap();
    assert!(iterator.next().unwrap().is_none());
    let mut block = section.read_value_changes().unwrap();
    let head = block.read_head_layout().unwrap();
    let tail = block.read_tail_layout().unwrap();
    let positions = block.read_positions(&head, &tail).unwrap();
    let times = block.read_time_table(&tail).unwrap();
    let bits = block.read_bits(&head).unwrap();
    let frame = bits
        .values(geometry)
        .map(|value| value.unwrap().to_ascii())
        .collect();
    let changes = (0..geometry.len())
        .map(|id| {
            let waves = block
                .read_waves(&head, &positions, id as u64, geometry[id])
                .unwrap();
            waves
                .iter()
                .map(|change| {
                    let (index, value) = change.unwrap();
                    (index, value.to_ascii())
                })
                .collect()
        })
        .collect();
    DecodedBlock {
        block_type: block.block_type,
        start_time: block.start_time,
        end_time: block.end_time,
        memory_required: block.memory_required,
        times,
        frame,
        positions,
        changes,
        tail,
        head,
    }
}

fn encode_block(block: &BlockBuffer, options: EncodeOptions) -> Vec<u8> {
    let mut out = Vec::new();
    BlockEncoder::new(options).encode(block, &mut out).unwrap();
    out
}

fn handle(index: usize) -> FstSignalHandle {
    FstSignalHandle::from_index(index)
}

fn ascii_changes(changes: &[(u64, &str)]) -> Vec<(u64, Vec<u8>)> {
    changes
        .iter()
        .map(|&(index, value)| (index, value.as_bytes().to_vec()))
        .collect()
}

#[test]
fn encoded_block_is_read_back_by_the_block_readers() {
    let geometry = [
        VariableLength::Bits(1),
        VariableLength::Bits(4),
        VariableLength::Real,
        VariableLength::VarLen,
        VariableLength::Bits(70),
    ];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    block.change_bit(handle(0), false).unwrap();
    block.push_time(10).unwrap();
    block.change_ascii(handle(1), b"10x1").unwrap();
    block.change_f64(handle(2), 1.5).unwrap();
    block.push_time(15).unwrap();
    block.change(handle(0), Value::Bit(b'z')).unwrap();
    block.change_var_len(handle(3), b"hello").unwrap();
    block.change_u64(handle(1), 0b0110).unwrap();
    block.push_time(15).unwrap();
    block.push_time(40).unwrap();
    block
        .change_words32(handle(4), &[1, 0, 0b110_0000])
        .unwrap();
    block.change_bit(handle(0), true).unwrap();

    let decoded = decode_block(&encode_block(&block, EncodeOptions::default()), &geometry);
    assert_eq!(decoded.block_type, BlockType::ValueChangeDataDynamicAlias2);
    assert_eq!((decoded.start_time, decoded.end_time), (10, 40));
    assert_eq!(decoded.times, [10, 15, 40]);
    assert_eq!(decoded.frame[0], b"x");
    assert_eq!(decoded.frame[1], b"xxxx");
    assert!(f64::from_le_bytes(decoded.frame[2][..].try_into().unwrap()).is_nan());
    assert_eq!(
        decoded.changes[0],
        ascii_changes(&[(0, "0"), (1, "z"), (2, "1")])
    );
    assert_eq!(
        decoded.changes[1],
        ascii_changes(&[(0, "10x1"), (1, "0110")])
    );
    assert_eq!(decoded.changes[2], [(0, 1.5f64.to_le_bytes().to_vec())]);
    assert_eq!(decoded.changes[3], ascii_changes(&[(1, "hello")]));
    let mut wide = vec![b'0'; 70];
    wide[0] = b'1';
    wide[69] = b'1';
    assert_eq!(decoded.changes[4], [(2, wide)]);
}

#[test]
fn every_input_form_records_the_same_wide_value() {
    let geometry = [VariableLength::Bits(40)];
    let value: u64 = 0x12_89AB_CDEF;
    let expected = format!("{value:040b}");
    let packed: Vec<u8> = (value << 24).to_be_bytes()[..5].to_vec();

    let mut block = BlockBuffer::new(&geometry).unwrap();
    let high = (value >> 32) as u32;
    block.push_time(0).unwrap();
    block.change_u64(handle(0), value | 0xFF << 56).unwrap();
    block.push_time(1).unwrap();
    block
        .change_words32(handle(0), &[value as u32, high | 0xF00])
        .unwrap();
    block.push_time(2).unwrap();
    block.change_words64(handle(0), &[value]).unwrap();
    block.push_time(3).unwrap();
    block
        .change_four_state(handle(0), &[value as u32, high], &[0, 0x100])
        .unwrap();
    block.push_time(4).unwrap();
    block
        .change(
            handle(0),
            Value::Packed {
                data: &packed,
                bits: 40,
            },
        )
        .unwrap();
    block.push_time(5).unwrap();
    block.change_ascii(handle(0), expected.as_bytes()).unwrap();

    let decoded = decode_block(&encode_block(&block, EncodeOptions::default()), &geometry);
    let values: Vec<_> = decoded.changes[0].iter().map(|(_, v)| v.clone()).collect();
    assert!(
        values.iter().all(|v| v == expected.as_bytes()),
        "{values:?}"
    );
    assert_eq!(values.len(), 6);
}

#[test]
fn four_state_planes_map_to_01zx() {
    let geometry = [VariableLength::Bits(4), VariableLength::Bits(1)];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    block.push_time(0).unwrap();
    block
        .change_four_state(handle(0), &[0b1010], &[0b1100])
        .unwrap();
    block.change_four_state(handle(1), &[0], &[1]).unwrap();

    let decoded = decode_block(&encode_block(&block, EncodeOptions::default()), &geometry);
    assert_eq!(decoded.changes[0], ascii_changes(&[(0, "xz10")]));
    assert_eq!(decoded.changes[1], ascii_changes(&[(0, "z")]));
}

#[test]
fn a_later_change_at_the_same_time_replaces_the_earlier_one() {
    let geometry = [VariableLength::Bits(8)];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    block.push_time(5).unwrap();
    block.change_u64(handle(0), 1).unwrap();
    block.change_u64(handle(0), 2).unwrap();
    block.push_time(6).unwrap();
    block.change_u64(handle(0), 3).unwrap();

    let decoded = decode_block(&encode_block(&block, EncodeOptions::default()), &geometry);
    assert_eq!(
        decoded.changes[0],
        ascii_changes(&[(0, "00000010"), (1, "00000011")])
    );
}

#[test]
fn deduplicate_drops_repeats_except_at_the_first_time_step() {
    let geometry = [
        VariableLength::Bits(1),
        VariableLength::Bits(3),
        VariableLength::VarLen,
    ];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    block.push_time(0).unwrap();
    block.change_ascii(handle(0), b"x").unwrap();
    block.change_ascii(handle(1), b"101").unwrap();
    block.change_var_len(handle(2), b"a").unwrap();
    for time in 1..4 {
        block.push_time(time).unwrap();
        block.change_ascii(handle(0), b"x").unwrap();
        block.change_u64(handle(1), 0b101).unwrap();
        block.change_var_len(handle(2), b"a").unwrap();
    }
    block.push_time(4).unwrap();
    block.change_bit(handle(0), true).unwrap();
    block.change_u64(handle(1), 0).unwrap();

    let options = EncodeOptions {
        deduplicate: true,
        ..EncodeOptions::default()
    };
    let decoded = decode_block(&encode_block(&block, options), &geometry);
    assert_eq!(decoded.changes[0], ascii_changes(&[(0, "x"), (4, "1")]));
    assert_eq!(decoded.changes[1], ascii_changes(&[(0, "101"), (4, "000")]));
    assert_eq!(
        decoded.changes[2],
        ascii_changes(&[(0, "a"), (1, "a"), (2, "a"), (3, "a")])
    );

    let mut next = BlockBuffer::new(&geometry).unwrap();
    block.start_next(&mut next).unwrap();
    next.push_time(5).unwrap();
    next.change_bit(handle(0), true).unwrap();
    next.change_ascii(handle(1), b"001").unwrap();
    let decoded = decode_block(&encode_block(&next, options), &geometry);
    assert_eq!(decoded.changes[0], []);
    assert_eq!(decoded.changes[1], ascii_changes(&[(1, "001")]));
}

#[test]
fn identical_waves_become_dynamic_aliases() {
    let geometry = [VariableLength::Bits(1); 5];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    for time in 0..4 {
        block.push_time(time).unwrap();
        let bit = time % 2 == 0;
        block.change_bit(handle(0), bit).unwrap();
        block.change_bit(handle(1), !bit).unwrap();
        block.change_bit(handle(2), bit).unwrap();
        block.change_bit(handle(4), bit).unwrap();
    }

    let decoded = decode_block(&encode_block(&block, EncodeOptions::default()), &geometry);
    assert!(matches!(decoded.positions[0], WavePosition::Data { .. }));
    assert!(matches!(decoded.positions[1], WavePosition::Data { .. }));
    assert_eq!(decoded.positions[2], WavePosition::Alias(0));
    assert_eq!(decoded.positions[3], WavePosition::None);
    assert_eq!(decoded.positions[4], WavePosition::Alias(0));
    assert_eq!(decoded.changes[4], decoded.changes[0]);
    // Each wave is 4 single-byte entries; the aliases are counted too.
    assert_eq!(decoded.memory_required, 4 * 4);
}

#[test]
fn start_next_frame_holds_the_final_values_of_the_previous_block() {
    let geometry = [
        VariableLength::Bits(1),
        VariableLength::Bits(12),
        VariableLength::Real,
        VariableLength::VarLen,
        VariableLength::Bits(2),
    ];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    block.push_time(3).unwrap();
    block.change_bit(handle(0), true).unwrap();
    block.change_u64(handle(1), 0xABC).unwrap();
    block.change_var_len(handle(3), b"s").unwrap();
    block.push_time(8).unwrap();
    block.change_f64(handle(2), -2.0).unwrap();
    block.change_ascii(handle(1), b"0000zzzz1111").unwrap();

    let mut next = BlockBuffer::new(&[VariableLength::Bits(1)]).unwrap();
    block.start_next(&mut next).unwrap();
    assert_eq!(next.times(), [8]);
    assert!(!next.has_time_step());
    next.push_time(9).unwrap();

    let decoded = decode_block(&encode_block(&next, EncodeOptions::default()), &geometry);
    assert_eq!(decoded.start_time, 8);
    assert_eq!(decoded.times, [8, 9]);
    assert_eq!(decoded.frame[0], b"1");
    assert_eq!(decoded.frame[1], b"0000zzzz1111");
    assert_eq!(decoded.frame[2], (-2.0f64).to_le_bytes());
    assert_eq!(decoded.frame[4], b"xx");
}

#[test]
fn long_waves_are_lz4_compressed_when_it_shrinks_them() {
    let geometry = [VariableLength::Bits(64), VariableLength::Bits(64)];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    for time in 0..100 {
        block.push_time(time).unwrap();
        block.change_u64(handle(0), time % 2).unwrap();
        block
            .change_u64(handle(1), time.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .unwrap();
    }
    let compressed = encode_block(&block, EncodeOptions::default());
    let raw = encode_block(
        &block,
        EncodeOptions {
            wave_compression: WaveCompression::None,
            ..EncodeOptions::default()
        },
    );
    assert!(compressed.len() < raw.len());

    let a = decode_block(&compressed, &geometry);
    let b = decode_block(&raw, &geometry);
    assert_eq!(a.changes, b.changes);
    assert_eq!(a.memory_required, b.memory_required);
    assert_eq!(a.head.pack_type, b'4');
}

#[test]
fn time_table_and_frame_are_compressed_only_when_strictly_smaller() {
    let geometry = [VariableLength::Bits(1)];
    let mut small = BlockBuffer::new(&geometry).unwrap();
    small.push_time(1).unwrap();
    let decoded = decode_block(&encode_block(&small, EncodeOptions::default()), &geometry);
    assert_eq!(
        decoded.tail.time_uncompressed_length,
        decoded.tail.time_compressed_length
    );
    assert_eq!(
        decoded.head.bits_uncompressed_length,
        decoded.head.bits_compressed_length
    );

    let geometry = [VariableLength::Bits(4096)];
    let mut large = BlockBuffer::new(&geometry).unwrap();
    for time in 0..1000 {
        large.push_time(time * 5).unwrap();
    }
    let decoded = decode_block(&encode_block(&large, EncodeOptions::default()), &geometry);
    assert!(decoded.tail.time_compressed_length < decoded.tail.time_uncompressed_length);
    assert!(decoded.head.bits_compressed_length < decoded.head.bits_uncompressed_length);
    assert_eq!(decoded.times.len(), 1000);
    assert_eq!(decoded.frame[0], vec![b'x'; 4096]);
}

#[test]
fn block_buffer_rejects_invalid_changes() {
    let geometry = [
        VariableLength::Bits(1),
        VariableLength::Bits(8),
        VariableLength::Real,
        VariableLength::VarLen,
        VariableLength::Bits(65),
    ];
    let mut block = BlockBuffer::new(&geometry).unwrap();
    block.push_time(10).unwrap();
    let invalid = |result: std::io::Result<()>| {
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    };
    invalid(block.change_bit(handle(5), true));
    invalid(block.change_ascii(handle(0), b"q"));
    invalid(block.change_ascii(handle(1), b"0101"));
    invalid(block.change_bit(handle(1), true));
    invalid(block.change_f64(handle(1), 1.0));
    invalid(block.change_u64(handle(4), 1));
    invalid(block.change_words32(handle(4), &[0, 0]));
    invalid(block.change_four_state(handle(1), &[0], &[]));
    invalid(block.change_ascii(handle(3), b"text"));
    invalid(block.change_var_len(handle(2), b"12345678"));
    invalid(block.change(
        handle(1),
        Value::Packed {
            data: &[0],
            bits: 7,
        },
    ));
    invalid(block.push_time(9));

    block.push_time(11).unwrap();
    block.change_bit(handle(0), true).unwrap();
    invalid(block.change_at(0, handle(0), Value::Bit(b'0')));
    invalid(block.change_at(2, handle(0), Value::Bit(b'0')));
    block
        .change_at(0, handle(1), Value::Ascii(b"00000000"))
        .unwrap();

    let mut next = BlockBuffer::new(&geometry).unwrap();
    block.start_next(&mut next).unwrap();
    invalid(next.change_bit(handle(0), false));

    invalid(BlockBuffer::new(&[VariableLength::Bits(0)]).map(drop));
    let empty = BlockBuffer::new(&geometry).unwrap();
    let mut out = Vec::new();
    invalid(BlockEncoder::new(EncodeOptions::default()).encode(&empty, &mut out));
    invalid(empty.start_next(&mut next));
}

/// A randomly generated change, with its expected ASCII value.
fn random_change(
    rng: &mut rand::rngs::StdRng,
    length: VariableLength,
    block: &mut BlockBuffer,
    signal: usize,
) -> Vec<u8> {
    use rand::RngExt;
    let h = handle(signal);
    match length {
        VariableLength::Real => {
            let value: f64 = rng.random_range(-1e3..1e3);
            block.change_f64(h, value).unwrap();
            value.to_le_bytes().to_vec()
        }
        VariableLength::VarLen => {
            let len = rng.random_range(0..12);
            let bytes: Vec<u8> = (0..len).map(|_| rng.random_range(b'a'..=b'c')).collect();
            block.change_var_len(h, &bytes).unwrap();
            bytes
        }
        VariableLength::Bits(bits) => {
            let bits = bits as usize;
            let four_state = rng.random_bool(0.3);
            let ascii: Vec<u8> = (0..bits)
                .map(|_| {
                    if four_state {
                        b"01xz"[rng.random_range(0..4)]
                    } else {
                        b"01"[rng.random_range(0..2)]
                    }
                })
                .collect();
            let words = |word_bits: usize| -> (Vec<u64>, Vec<u64>) {
                let count = bits.div_ceil(word_bits);
                let mut aval = vec![0u64; count];
                let mut bval = vec![0u64; count];
                for (i, &c) in ascii.iter().rev().enumerate() {
                    let (a, b) = match c {
                        b'0' => (0, 0),
                        b'1' => (1, 0),
                        b'z' => (0, 1),
                        _ => (1, 1),
                    };
                    aval[i / word_bits] |= a << (i % word_bits);
                    bval[i / word_bits] |= b << (i % word_bits);
                }
                (aval, bval)
            };
            match rng.random_range(0..6) {
                0 => block.change_ascii(h, &ascii).unwrap(),
                1 if four_state => {
                    let (aval, bval) = words(32);
                    let aval: Vec<u32> = aval.iter().map(|&w| w as u32).collect();
                    let bval: Vec<u32> = bval.iter().map(|&w| w as u32).collect();
                    block.change_four_state(h, &aval, &bval).unwrap();
                }
                _ if four_state => block.change(h, Value::Ascii(&ascii)).unwrap(),
                2 if bits <= 64 => block.change_u64(h, words(64).0[0]).unwrap(),
                3 => {
                    let words: Vec<u32> = words(32).0.iter().map(|&w| w as u32).collect();
                    block.change_words32(h, &words).unwrap();
                }
                4 => block.change_words64(h, &words(64).0).unwrap(),
                _ => {
                    let mut packed = vec![0u8; bits.div_ceil(8)];
                    for (i, &c) in ascii.iter().enumerate() {
                        packed[i / 8] |= (c & 1) << (7 - i % 8);
                    }
                    block
                        .change(
                            h,
                            Value::Packed {
                                data: &packed,
                                bits,
                            },
                        )
                        .unwrap();
                }
            }
            ascii
        }
    }
}

#[test]
fn random_blocks_round_trip_through_the_block_readers() {
    use rand::{RngExt, SeedableRng};
    for seed in 0..40 {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let geometry: Vec<VariableLength> = (0..rng.random_range(1..30))
            .map(|_| match rng.random_range(0..10) {
                0 => VariableLength::Real,
                1 => VariableLength::VarLen,
                2..=5 => VariableLength::Bits(1),
                _ => VariableLength::Bits(rng.random_range(2..140)),
            })
            .collect();

        let mut block = BlockBuffer::new(&geometry).unwrap();
        let mut expected: Vec<Vec<(u64, Vec<u8>)>> = vec![Vec::new(); geometry.len()];
        let mut time = rng.random_range(0..100);
        for time_index in 0..rng.random_range(1..60u64) {
            block.push_time(time).unwrap();
            time += rng.random_range(1..1000);
            for _ in 0..rng.random_range(0..geometry.len() * 2) {
                let signal = rng.random_range(0..geometry.len());
                let ascii = random_change(&mut rng, geometry[signal], &mut block, signal);
                let changes = &mut expected[signal];
                if changes
                    .last()
                    .is_some_and(|(index, _)| *index == time_index)
                {
                    changes.pop();
                }
                changes.push((time_index, ascii));
            }
        }

        let decoded = decode_block(&encode_block(&block, EncodeOptions::default()), &geometry);
        assert_eq!(decoded.times, block.times(), "seed {seed}");
        assert_eq!(decoded.changes, expected, "seed {seed}");
    }
}
