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
