use std::io::{BufRead, Read, Seek, Take};

use flate2::read::{GzDecoder, ZlibDecoder};

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
        println!(
            "ValueChange: type={:?} time={}..{} memory_required={} vars={} pack_type={:?} bits_count={} time_count={}",
            block.block_type,
            block.start_time,
            block.end_time,
            block.memory_required,
            block.waves_count,
            block.pack_type as char,
            block.bits_count,
            block.time_count,
        );

        let times = block.read_time_table()?;
        let bits = block.read_bits()?;
        for (id, value) in bits.values(&lengths).take(4).enumerate() {
            println!("  initial [{id}] {}", value?.display(real_endianness));
        }

        for id in 0..lengths.len().min(4) as u64 {
            let waves = block.waves(id, lengths[id as usize])?;
            for change in waves.iter().take(5) {
                let (time_index, value) = change?;
                let time = times[time_index as usize];
                println!("  #{time} [{id}] {}", value.display(real_endianness));
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BlockType {
    /// Header block, found at the start of the file.
    ///
    /// FST_BL_HDR
    Header = 0,
    /// Value Change data. Records the actual waveforms for a block of time.
    ///
    /// FST_BL_VCDATA
    ValueChangeData = 1,
    /// Stores the times when $dumpoff/on was called.
    ///
    /// FST_BL_BLACKOUT
    Blackout = 2,
    /// Stores the length of each variable.
    ///
    /// FST_BL_GEOM
    Geometry = 3,
    /// Hierarchy data (names of modules, wires etc.)
    ///
    /// FST_BL_HIER
    Hierarchy = 4,
    /// Newer version of FST_BL_VCDATA.
    ///
    /// FST_BL_VCDATA_DYN_ALIAS
    ValueChangeDataDynamicAlias = 5,
    /// Hierarchy data compressed with LZ4
    ///
    /// FST_BL_HIER_LZ4
    HierarchyLZ4 = 6,
    /// Hierarchy data compressed with LZ4 twice. This gives better compression.
    ///
    /// FST_BL_HIER_LZ4DUO
    HierarchyLZ4Duo = 7,
    /// Even newer version of FST_BL_VCDATA.
    ///
    /// FST_BL_VCDATA_DYN_ALIAS2
    ValueChangeDataDynamicAlias2 = 8,
    /// This block is an entire FST file that has been GZipped.
    ///
    /// FST_BL_ZWRAPPER
    GzippedWrapper = 254,
    /// Value Change blocks are set to this type while being written.
    ///
    /// FST_BL_SKIP
    Skip = 255,
}

/// A section of the file, borrowed from a [`SectionIterator`].
///
/// The caller decides whether to read the section data (through [`Read`] or one
/// of the `read_*` methods) or to [`skip`](Section::skip) it. Dropping the
/// section without fully reading it is also fine: the iterator skips whatever
/// is left before reading the next section.
struct Section<'a, R> {
    block_type: u8,
    len: u64,
    /// The iterator's reader, limited to the remaining bytes of this section.
    data: &'a mut Take<R>,
}

#[derive(Debug, Clone, Copy)]
pub enum RealEndianess {
    LittleEndian,
    BigEndian,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HeaderBlock {
    /// Start time of the file. Units are given by timescale.
    pub start_time: u64,

    /// End time of the file.
    pub end_time: u64,

    /// Endianess of real values encoded in the file.
    pub real_endianness: RealEndianess,

    /// Memory used when writing this file in bytes.
    pub writer_memory_use: u64,

    /// Number of scopes (FST_ST_VCD_SCOPE entries in the hierarchy).
    pub num_scopes: u64,

    /// Number of variables in the hierarchy.
    pub num_hierarchy_vars: u64,

    /// Number of variables that are distinct (not structurally equivalent).
    pub num_vars: u64,

    /// Number of Value Change blocks in the file.
    pub num_vc_blocks: u64,

    /// Order of magnitude of the time unit. 0=1s, -9=1ns, etc.
    pub timescale: i8,

    /// Simulator identifier. Null terminated if shorter than 128 bytes.
    pub writer: [u8; 128],

    /// Null terminated date string as returned by asctime().
    pub date: [u8; 26],

    /// Reserved for future use. Should be filled with zeros.
    /// Calculated as: Offset 321 (filetype) - Offset 228 (start of reserved) = 93 bytes.
    pub reserved: [u8; 93],

    /// File type (e.g., FST_FT_VERILOG).
    pub file_type: u8,

    /// Timezero ($timezero in a VCD file).
    /// Used to shift time if the simulation start time is negative.
    pub timezero: i64,
}

impl<R: Read> Read for Section<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.data.read(buf)
    }
}

impl<R> Section<'_, R> {
    /// Length of the section data, in bytes (excluding block type and length fields).
    fn len(&self) -> u64 {
        self.len
    }

    fn block_type(&self) -> Option<BlockType> {
        match self.block_type {
            0 => Some(BlockType::Header),
            1 => Some(BlockType::ValueChangeData),
            2 => Some(BlockType::Blackout),
            3 => Some(BlockType::Geometry),
            4 => Some(BlockType::Hierarchy),
            5 => Some(BlockType::ValueChangeDataDynamicAlias),
            6 => Some(BlockType::HierarchyLZ4),
            7 => Some(BlockType::HierarchyLZ4Duo),
            8 => Some(BlockType::ValueChangeDataDynamicAlias2),
            254 => Some(BlockType::GzippedWrapper),
            255 => Some(BlockType::Skip),
            _ => None,
        }
    }
}

impl<R: Read + Seek> Section<'_, R> {
    /// Reads the section as a header block.
    fn read_header(mut self) -> std::io::Result<HeaderBlock> {
        if self.block_type != BlockType::Header as u8 {
            return Err(invalid_data("section is not a header block"));
        }

        if self.len != 321 {
            return Err(invalid_data("unexpected header block length"));
        }

        let mut data = [0u8; 321];
        self.read_exact(&mut data)?;

        let start_time = u64::from_be_bytes(data[0..8].try_into().unwrap());
        let end_time = u64::from_be_bytes(data[8..16].try_into().unwrap());

        let real_endianness = 'endianness: {
            let v = u64::from_le_bytes(data[16..24].try_into().unwrap());
            let real_endianness = f64::from_bits(v);
            if real_endianness == std::f64::consts::E {
                break 'endianness RealEndianess::LittleEndian;
            }

            let v = u64::from_be_bytes(data[16..24].try_into().unwrap());
            let real_endianness = f64::from_bits(v);
            if real_endianness == std::f64::consts::E {
                break 'endianness RealEndianess::BigEndian;
            }

            return Err(invalid_data("invalid real endianness marker"));
        };

        let writer_memory_use = u64::from_be_bytes(data[24..32].try_into().unwrap());
        let num_scopes = u64::from_be_bytes(data[32..40].try_into().unwrap());
        let num_hierarchy_vars = u64::from_be_bytes(data[40..48].try_into().unwrap());
        let num_vars = u64::from_be_bytes(data[48..56].try_into().unwrap());
        let num_vc_blocks = u64::from_be_bytes(data[56..64].try_into().unwrap());
        let timescale = data[64] as i8;
        let writer: [u8; 128] = data[65..193].try_into().unwrap();
        let date: [u8; 26] = data[193..219].try_into().unwrap();
        let reserved: [u8; 93] = data[219..312].try_into().unwrap();
        let file_type = data[312];
        let timezero = i64::from_be_bytes(data[313..321].try_into().unwrap());

        Ok(HeaderBlock {
            start_time,
            end_time,
            real_endianness,
            writer_memory_use,
            num_scopes,
            num_hierarchy_vars,
            num_vars,
            num_vc_blocks,
            timescale,
            writer,
            date,
            reserved,
            file_type,
            timezero,
        })
    }
}

impl<'a, R: Read> Section<'a, R> {
    /// Reads the section as a geometry block.
    ///
    /// Only the fixed size fields are read; the length entries are read lazily
    /// through [`GeometryBlock::lengths`].
    fn read_geometry(mut self) -> std::io::Result<GeometryBlock<'a, R>> {
        if self.block_type != BlockType::Geometry as u8 {
            return Err(invalid_data("section is not a geometry block"));
        }

        let mut fields = [0u8; 16];
        self.read_exact(&mut fields)?;

        let uncompressed_length = u64::from_be_bytes(fields[0..8].try_into().unwrap());
        let count = u64::from_be_bytes(fields[8..16].try_into().unwrap());

        Ok(GeometryBlock {
            uncompressed_length,
            compressed_length: self.data.limit(),
            count,
            data: self.data,
        })
    }
}

/// Geometry block: the length of each variable in bits.
struct GeometryBlock<'a, R> {
    /// Length of the uncompressed data. Equal to `compressed_length` if the data
    /// is not compressed.
    uncompressed_length: u64,
    /// Length of the (possibly compressed) data.
    compressed_length: u64,
    /// Number of length entries in the data.
    count: u64,
    /// The section reader, limited to the geometry data.
    data: &'a mut Take<R>,
}

impl<'a, R: Read> GeometryBlock<'a, R> {
    fn is_compressed(&self) -> bool {
        self.uncompressed_length != self.compressed_length
    }

    /// Returns an iterator over the length of each variable.
    fn lengths(self) -> GeometryLengths<'a, R> {
        let data = if self.is_compressed() {
            GeometryData::Zlib(ZlibDecoder::new(self.data))
        } else {
            GeometryData::Raw(self.data)
        };

        GeometryLengths {
            data,
            remaining: self.count,
        }
    }
}

/// Length of a variable, as recorded in the geometry block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariableLength {
    /// Length in bits.
    Bits(u64),
    /// A real variable (encoded as 0).
    Real,
    /// A variable length value, such as a string (encoded as 0xFFFFFFFF).
    ///
    /// Has no fixed length, so it is not stored in the bits array.
    VarLen,
}

/// The geometry data, decompressed if needed.
enum GeometryData<'a, R> {
    Raw(&'a mut Take<R>),
    Zlib(ZlibDecoder<&'a mut Take<R>>),
}

impl<R: Read> Read for GeometryData<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            GeometryData::Raw(data) => data.read(buf),
            GeometryData::Zlib(data) => data.read(buf),
        }
    }
}

struct GeometryLengths<'a, R> {
    data: GeometryData<'a, R>,
    /// Entries not yet read.
    remaining: u64,
}

impl<R: Read> Iterator for GeometryLengths<'_, R> {
    type Item = std::io::Result<VariableLength>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let length = match read_varint(&mut self.data) {
            Ok(0) => VariableLength::Real,
            Ok(0xFFFF_FFFF) => VariableLength::VarLen,
            Ok(bits) => VariableLength::Bits(bits),
            Err(err) => {
                // Don't keep reading after an error.
                self.remaining = 0;
                return Some(Err(err));
            }
        };
        Some(Ok(length))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        (0, Some(remaining))
    }
}

/// Reads an unsigned LEB128 varint.
fn read_varint<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        let mut byte = 0u8;
        reader.read_exact(std::slice::from_mut(&mut byte))?;

        if shift >= 64 || (shift == 63 && byte & 0x7F > 1) {
            return Err(invalid_data("varint overflows u64"));
        }
        value |= u64::from(byte & 0x7F) << shift;
        shift += 7;

        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
}

impl<'a, R: Read> Section<'a, R> {
    /// Reads the section as a hierarchy block.
    ///
    /// Only the fixed size fields are read; the entries are decompressed and
    /// read by [`HierarchyBlock::entries`].
    fn read_hierarchy(mut self) -> std::io::Result<HierarchyBlock<'a, R>> {
        let mut uncompressed_length = [0u8; 8];
        self.read_exact(&mut uncompressed_length)?;
        let uncompressed_length = u64::from_be_bytes(uncompressed_length);

        let compression = match self.block_type() {
            Some(BlockType::Hierarchy) => HierarchyCompression::Gzip,
            Some(BlockType::HierarchyLZ4) => HierarchyCompression::Lz4,
            Some(BlockType::HierarchyLZ4Duo) => HierarchyCompression::Lz4Duo {
                compressed_once_length: read_varint(&mut self)?,
            },
            _ => return Err(invalid_data("section is not a hierarchy block")),
        };

        Ok(HierarchyBlock {
            compression,
            uncompressed_length,
            data: self.data,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HierarchyCompression {
    /// FST_BL_HIER
    Gzip,
    /// FST_BL_HIER_LZ4
    Lz4,
    /// FST_BL_HIER_LZ4DUO: LZ4 applied twice.
    Lz4Duo {
        /// Length of the data after the first LZ4 compression.
        compressed_once_length: u64,
    },
}

/// Hierarchy block: the design hierarchy and all signal names.
struct HierarchyBlock<'a, R> {
    compression: HierarchyCompression,
    /// Length of the hierarchy data after decompression.
    uncompressed_length: u64,
    /// The section reader, limited to the compressed hierarchy data.
    data: &'a mut Take<R>,
}

impl<'a, R: Read> HierarchyBlock<'a, R> {
    /// Returns an iterator over the hierarchy entries.
    ///
    /// Gzip data is decompressed while iterating. LZ4 data can only be
    /// decompressed as a whole, so it is read and decompressed here.
    fn entries(self) -> std::io::Result<HierarchyEntries<'a, R>> {
        let data = match self.compression {
            HierarchyCompression::Gzip => {
                HierarchyData::Gzip(std::io::BufReader::new(GzDecoder::new(self.data)))
            }
            HierarchyCompression::Lz4 => {
                let compressed = read_to_vec(self.data)?;
                let data = lz4_decompress(&compressed, self.uncompressed_length)?;
                HierarchyData::Buffer(std::io::Cursor::new(data))
            }
            HierarchyCompression::Lz4Duo {
                compressed_once_length,
            } => {
                let compressed = read_to_vec(self.data)?;
                let once = lz4_decompress(&compressed, compressed_once_length)?;
                let data = lz4_decompress(&once, self.uncompressed_length)?;
                HierarchyData::Buffer(std::io::Cursor::new(data))
            }
        };

        Ok(HierarchyEntries {
            data,
            next_id: 0,
            done: false,
        })
    }
}

fn read_to_vec<R: Read>(reader: &mut Take<R>) -> std::io::Result<Vec<u8>> {
    let len = usize::try_from(reader.limit()).map_err(|_| invalid_data("section too large"))?;
    let mut data = Vec::with_capacity(len);
    reader.read_to_end(&mut data)?;
    if data.len() != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(data)
}

/// Decompresses a LZ4 block whose decompressed size must be `len`.
fn lz4_decompress(input: &[u8], len: u64) -> std::io::Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| invalid_data("hierarchy too large"))?;
    let mut output = vec![0u8; len];
    let written = lz4_flex::block::decompress_into(input, &mut output)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if written != len {
        return Err(invalid_data("LZ4 data shorter than expected"));
    }
    Ok(output)
}

/// The hierarchy data, after decompression.
enum HierarchyData<'a, R> {
    Gzip(std::io::BufReader<GzDecoder<&'a mut Take<R>>>),
    Buffer(std::io::Cursor<Vec<u8>>),
}

impl<R: Read> Read for HierarchyData<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            HierarchyData::Gzip(data) => data.read(buf),
            HierarchyData::Buffer(data) => data.read(buf),
        }
    }
}

impl<R: Read> BufRead for HierarchyData<'_, R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        match self {
            HierarchyData::Gzip(data) => data.fill_buf(),
            HierarchyData::Buffer(data) => data.fill_buf(),
        }
    }

    fn consume(&mut self, amount: usize) {
        match self {
            HierarchyData::Gzip(data) => data.consume(amount),
            HierarchyData::Buffer(data) => data.consume(amount),
        }
    }
}

/// FST_ST_GEN_ATTRBEGIN
const TAG_ATTRIBUTE_BEGIN: u8 = 252;
/// FST_ST_GEN_ATTREND
const TAG_ATTRIBUTE_END: u8 = 253;
/// FST_ST_VCD_SCOPE
const TAG_SCOPE: u8 = 254;
/// FST_ST_VCD_UPSCOPE
const TAG_UPSCOPE: u8 = 255;
/// FST_VT_MAX: tags up to this value are variable types (FST_VT_*).
const TAG_VAR_MAX: u8 = 29;
/// FST_VT_VCD_PORT
const VAR_TYPE_PORT: u8 = 18;

/// An entry of the hierarchy data.
///
/// Names are kept as raw bytes, since they are not guaranteed to be UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HierarchyEntry {
    /// FST_ST_GEN_ATTRBEGIN: begins an attribute for the current scope.
    AttributeBegin {
        /// See FST_AT_*.
        attr_type: u8,
        /// See FST_MT_* (meaning depends on `attr_type`).
        subtype: u8,
        name: Vec<u8>,
        value: u64,
    },
    /// FST_ST_GEN_ATTREND
    AttributeEnd,
    /// FST_ST_VCD_SCOPE: enters a new scope.
    Scope {
        /// See FST_ST_*.
        scope_type: u8,
        name: Vec<u8>,
        component: Vec<u8>,
    },
    /// FST_ST_VCD_UPSCOPE: ends the current scope.
    UpScope,
    /// FST_VT_*: a variable.
    Var {
        /// See FST_VT_*.
        var_type: u8,
        /// See FST_VD_*.
        direction: u8,
        name: Vec<u8>,
        /// Length in bits. For FST_VT_VCD_PORT this is the raw value, which
        /// must be interpreted differently.
        length: u64,
        /// Variable ID. For an alias, this is the ID of the aliased variable.
        id: u64,
        /// Whether this variable is a structural alias of variable `id`.
        is_alias: bool,
    },
}

impl std::fmt::Display for HierarchyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match self {
            HierarchyEntry::AttributeBegin {
                attr_type,
                subtype,
                name,
                value,
            } => write!(
                f,
                "attr type={attr_type} subtype={subtype} {:?} = {value}",
                s(name)
            ),
            HierarchyEntry::AttributeEnd => write!(f, "attr end"),
            HierarchyEntry::Scope {
                scope_type,
                name,
                component,
            } => write!(
                f,
                "scope type={scope_type} {:?} {:?}",
                s(name),
                s(component)
            ),
            HierarchyEntry::UpScope => write!(f, "upscope"),
            HierarchyEntry::Var {
                var_type,
                direction,
                name,
                length,
                id,
                is_alias,
            } => {
                let port = if *var_type == VAR_TYPE_PORT {
                    " (port)"
                } else {
                    ""
                };
                let alias = if *is_alias { " alias" } else { "" };
                write!(
                    f,
                    "var type={var_type} dir={direction} {:?} length={length}{port} id={id}{alias}",
                    s(name)
                )
            }
        }
    }
}

struct HierarchyEntries<'a, R> {
    data: HierarchyData<'a, R>,
    /// ID of the next non-aliased variable.
    next_id: u64,
    /// Set at the end of the data or after an error.
    done: bool,
}

impl<R: Read> HierarchyEntries<'_, R> {
    fn read_entry(&mut self) -> std::io::Result<Option<HierarchyEntry>> {
        let tag = match self.data.fill_buf()?.first() {
            Some(&tag) => tag,
            None => return Ok(None),
        };
        self.data.consume(1);

        let entry = match tag {
            TAG_ATTRIBUTE_BEGIN => HierarchyEntry::AttributeBegin {
                attr_type: read_u8(&mut self.data)?,
                subtype: read_u8(&mut self.data)?,
                name: read_name(&mut self.data)?,
                value: read_varint(&mut self.data)?,
            },
            TAG_ATTRIBUTE_END => HierarchyEntry::AttributeEnd,
            TAG_SCOPE => HierarchyEntry::Scope {
                scope_type: read_u8(&mut self.data)?,
                name: read_name(&mut self.data)?,
                component: read_name(&mut self.data)?,
            },
            TAG_UPSCOPE => HierarchyEntry::UpScope,
            var_type @ 0..=TAG_VAR_MAX => {
                let direction = read_u8(&mut self.data)?;
                let name = read_name(&mut self.data)?;
                let length = read_varint(&mut self.data)?;
                let alias = read_varint(&mut self.data)?;

                let (id, is_alias) = if alias == 0 {
                    let id = self.next_id;
                    self.next_id += 1;
                    (id, false)
                } else {
                    (alias - 1, true)
                };

                HierarchyEntry::Var {
                    var_type,
                    direction,
                    name,
                    length,
                    id,
                    is_alias,
                }
            }
            _ => return Err(invalid_data("unknown hierarchy tag")),
        };

        Ok(Some(entry))
    }
}

impl<R: Read> Iterator for HierarchyEntries<'_, R> {
    type Item = std::io::Result<HierarchyEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let entry = self.read_entry().transpose();
        if !matches!(entry, Some(Ok(_))) {
            self.done = true;
        }
        entry
    }
}

fn read_u8<R: Read>(reader: &mut R) -> std::io::Result<u8> {
    let mut byte = 0u8;
    reader.read_exact(std::slice::from_mut(&mut byte))?;
    Ok(byte)
}

/// Reads a null terminated name, without the terminator.
fn read_name<R: BufRead>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut name = Vec::new();
    reader.read_until(0, &mut name)?;
    if name.pop() != Some(0) {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(name)
}

impl<'a, R: Read + Seek> Section<'a, R> {
    /// Reads the section as a value change block.
    ///
    /// Reads the fixed size fields and the trailers at the end of the section,
    /// which locate the tables. The tables are read on demand.
    fn read_value_changes(mut self) -> std::io::Result<ValueChangeBlock<'a, R>> {
        let block_type = match self.block_type() {
            Some(
                block_type @ (BlockType::ValueChangeData
                | BlockType::ValueChangeDataDynamicAlias
                | BlockType::ValueChangeDataDynamicAlias2),
            ) => block_type,
            _ => return Err(invalid_data("section is not a value change block")),
        };

        let mut fields = [0u8; 24];
        self.read_exact(&mut fields)?;
        let start_time = u64::from_be_bytes(fields[0..8].try_into().unwrap());
        let end_time = u64::from_be_bytes(fields[8..16].try_into().unwrap());
        let memory_required = u64::from_be_bytes(fields[16..24].try_into().unwrap());

        let bits_uncompressed_length = read_varint(&mut self)?;
        let bits_compressed_length = read_varint(&mut self)?;
        let bits_count = read_varint(&mut self)?;

        let mut block = ValueChangeBlock {
            block_type,
            start_time,
            end_time,
            memory_required,
            bits_uncompressed_length,
            bits_compressed_length,
            bits_count,
            waves_count: 0,
            pack_type: 0,
            time_uncompressed_length: 0,
            time_compressed_length: 0,
            time_count: 0,
            bits_start: 0,
            waves_start: 0,
            positions_start: 0,
            time_start: 0,
            positions: None,
            len: self.len,
            data: self.data,
        };

        block.bits_start = block.position();
        block.seek_to(checked_add(block.bits_start, bits_compressed_length)?)?;
        block.waves_count = read_varint(block.data)?;
        block.waves_start = block.position();
        block.pack_type = read_u8(block.data)?;

        // The time table trailer is at the end of the section.
        let trailer_start = block
            .len
            .checked_sub(24)
            .ok_or_else(|| invalid_data("value change block too short"))?;
        block.seek_to(trailer_start)?;
        let mut trailer = [0u8; 24];
        block.data.read_exact(&mut trailer)?;
        block.time_uncompressed_length = u64::from_be_bytes(trailer[0..8].try_into().unwrap());
        block.time_compressed_length = u64::from_be_bytes(trailer[8..16].try_into().unwrap());
        block.time_count = u64::from_be_bytes(trailer[16..24].try_into().unwrap());

        // The position table length is just before the time table.
        block.time_start = trailer_start
            .checked_sub(block.time_compressed_length)
            .ok_or_else(|| invalid_data("time table larger than block"))?;
        let positions_length_start = block
            .time_start
            .checked_sub(8)
            .ok_or_else(|| invalid_data("time table larger than block"))?;
        block.seek_to(positions_length_start)?;
        let mut positions_length = [0u8; 8];
        block.data.read_exact(&mut positions_length)?;
        let positions_length = u64::from_be_bytes(positions_length);

        block.positions_start = positions_length_start
            .checked_sub(positions_length)
            .filter(|&start| start > block.waves_start)
            .ok_or_else(|| invalid_data("position table larger than block"))?;

        Ok(block)
    }
}

/// Value change block: the value of every variable over a period of time.
///
/// Contains four tables, read on demand: the bits array (values at
/// `start_time`), the waves (the value changes of each variable), the position
/// table (where the waves of each variable are) and the time table.
struct ValueChangeBlock<'a, R> {
    block_type: BlockType,
    /// Start time of the block, in units of the header timescale.
    start_time: u64,
    end_time: u64,
    /// Buffer memory required for a full traversal of the block.
    memory_required: u64,
    bits_uncompressed_length: u64,
    /// Equal to `bits_uncompressed_length` if not compressed.
    bits_compressed_length: u64,
    /// Number of variables in the bits array.
    bits_count: u64,
    /// Number of variables in the position table.
    waves_count: u64,
    /// Compression of the waves: b'Z' (zlib), b'F' (FastLZ) or b'4' (LZ4).
    pack_type: u8,
    time_uncompressed_length: u64,
    /// Equal to `time_uncompressed_length` if not compressed.
    time_compressed_length: u64,
    /// Number of entries in the time table.
    time_count: u64,

    // Offsets of the tables, relative to the start of the section data.
    bits_start: u64,
    /// Offset of the pack type byte. Wave positions are relative to it.
    waves_start: u64,
    positions_start: u64,
    time_start: u64,

    /// The decoded position table, read on first use.
    positions: Option<Vec<WavePosition>>,

    /// Length of the section data.
    len: u64,
    /// The section reader.
    data: &'a mut Take<R>,
}

/// Where the waves of a variable are, as decoded from the position table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WavePosition {
    /// The variable has no value changes in this block.
    None,
    /// The waves are at `offset` from the pack type byte, and are `length`
    /// bytes long.
    Data { offset: u64, length: u64 },
    /// Dynamic alias: the variable has the same waves as variable `id`.
    Alias(u64),
}

impl<'a, R: Read + Seek> ValueChangeBlock<'a, R> {
    /// Current position, relative to the start of the section data.
    fn position(&self) -> u64 {
        self.len - self.data.limit()
    }

    /// Seeks to `position`, relative to the start of the section data.
    fn seek_to(&mut self, position: u64) -> std::io::Result<()> {
        if position > self.len {
            return Err(invalid_data("offset past end of value change block"));
        }
        let offset = i64::try_from(position).map_err(|_| invalid_data("section too large"))?
            - i64::try_from(self.position()).map_err(|_| invalid_data("section too large"))?;
        self.data.get_mut().seek_relative(offset)?;
        self.data.set_limit(self.len - position);
        Ok(())
    }

    /// Reads `length` bytes at `position`.
    fn read_at(&mut self, position: u64, length: u64) -> std::io::Result<Vec<u8>> {
        if checked_add(position, length)? > self.len {
            return Err(invalid_data("table past end of value change block"));
        }
        self.seek_to(position)?;
        let mut data = vec![0; to_usize(length)?];
        self.data.read_exact(&mut data)?;
        Ok(data)
    }

    /// Reads the bits array: the value of each variable at `start_time`.
    fn read_bits(&mut self) -> std::io::Result<BitsArray> {
        let data = self.read_at(self.bits_start, self.bits_compressed_length)?;
        let data = if self.bits_compressed_length != self.bits_uncompressed_length {
            zlib_decompress(&data, self.bits_uncompressed_length)?
        } else {
            data
        };
        Ok(BitsArray {
            data,
            count: self.bits_count,
        })
    }

    /// Reads the time table, returning the time of each time index.
    fn read_time_table(&mut self) -> std::io::Result<Vec<u64>> {
        let data = self.read_at(self.time_start, self.time_compressed_length)?;
        let data = if self.time_compressed_length != self.time_uncompressed_length {
            zlib_decompress(&data, self.time_uncompressed_length)?
        } else {
            data
        };

        let mut reader = &data[..];
        let mut time = 0u64;
        let mut times = Vec::with_capacity(to_usize(self.time_count)?.min(data.len()));
        for _ in 0..self.time_count {
            time = time
                .checked_add(read_varint(&mut reader)?)
                .ok_or_else(|| invalid_data("time overflows u64"))?;
            times.push(time);
        }
        Ok(times)
    }

    /// Returns the position table, reading it on first use.
    ///
    /// Has `waves_count` entries, one for each variable.
    fn positions(&mut self) -> std::io::Result<&[WavePosition]> {
        if self.positions.is_none() {
            let length = self.time_start - 8 - self.positions_start;
            let data = self.read_at(self.positions_start, length)?;
            let waves_end = self.positions_start - self.waves_start;
            let positions = match self.block_type {
                BlockType::ValueChangeDataDynamicAlias2 => {
                    decode_positions_alias2(&data, self.waves_count, waves_end)?
                }
                _ => decode_positions(&data, self.waves_count, waves_end)?,
            };
            self.positions = Some(positions);
        }
        Ok(self.positions.as_deref().unwrap())
    }

    /// Reads and decompresses the waves of variable `id`, following dynamic
    /// aliases.
    ///
    /// `length` is the variable length from the geometry block, needed to
    /// decode the waves.
    fn waves(&mut self, id: u64, length: VariableLength) -> std::io::Result<Waves> {
        let positions = self.positions()?;

        let mut position = *positions
            .get(to_usize(id)?)
            .ok_or_else(|| invalid_data("variable id out of range"))?;
        let mut current = id;
        while let WavePosition::Alias(target) = position {
            // Aliases always point backwards, which also rules out cycles.
            if target >= current {
                return Err(invalid_data("dynamic alias to a later variable"));
            }
            current = target;
            position = positions[to_usize(target)?];
        }

        let data = match position {
            WavePosition::None | WavePosition::Alias(_) => Vec::new(),
            WavePosition::Data { offset, length } => {
                let raw = self.read_at(self.waves_start + offset, length)?;
                let mut reader = &raw[..];
                let uncompressed_length = read_varint(&mut reader)?;
                if uncompressed_length == 0 {
                    reader.to_vec()
                } else {
                    match self.pack_type {
                        b'4' => lz4_decompress(reader, uncompressed_length)?,
                        b'F' => todo!("FastLZ decompression of value change data"),
                        // The C library treats anything else as zlib.
                        _ => zlib_decompress(reader, uncompressed_length)?,
                    }
                }
            }
        };

        Ok(Waves { data, length })
    }
}

/// Decodes the position table of FST_BL_VCDATA and FST_BL_VCDATA_DYN_ALIAS
/// blocks.
fn decode_positions(data: &[u8], count: u64, waves_end: u64) -> std::io::Result<Vec<WavePosition>> {
    let mut reader = data;
    let mut entries = Vec::new();
    let mut offset = 0u64;
    while !reader.is_empty() {
        let value = read_varint(&mut reader)?;
        if value == 0 {
            // A dynamic alias, as variable id + 1.
            let alias = read_varint(&mut reader)?;
            let id = alias
                .checked_sub(1)
                .ok_or_else(|| invalid_data("dynamic alias to variable 0"))?;
            entries.push(PositionEntry::Alias(id));
        } else if value & 1 == 1 {
            offset = checked_add(offset, value >> 1)?;
            entries.push(PositionEntry::Offset(offset));
        } else {
            entries.push(PositionEntry::Zeros(value >> 1));
        }
    }
    resolve_positions(entries, count, waves_end)
}

/// Decodes the position table of FST_BL_VCDATA_DYN_ALIAS2 blocks.
fn decode_positions_alias2(
    data: &[u8],
    count: u64,
    waves_end: u64,
) -> std::io::Result<Vec<WavePosition>> {
    let mut reader = data;
    let mut entries = Vec::new();
    let mut offset = 0u64;
    let mut previous_alias = None;
    while let Some(&first) = reader.first() {
        if first & 1 == 0 {
            entries.push(PositionEntry::Zeros(read_varint(&mut reader)? >> 1));
            continue;
        }

        let value = read_svarint(&mut reader)? >> 1;
        if value > 0 {
            offset = checked_add(offset, value as u64)?;
            entries.push(PositionEntry::Offset(offset));
        } else {
            let alias = if value < 0 {
                previous_alias = Some(value);
                value
            } else {
                // 0 repeats the previous alias.
                previous_alias
                    .ok_or_else(|| invalid_data("repeated alias without a previous one"))?
            };
            // The alias is -(id + 1).
            entries.push(PositionEntry::Alias((-(alias + 1)) as u64));
        }
    }
    resolve_positions(entries, count, waves_end)
}

/// A decoded position table entry, before the wave lengths are known.
enum PositionEntry {
    /// A run of variables without value changes.
    Zeros(u64),
    /// Offset of the waves, relative to the pack type byte.
    Offset(u64),
    Alias(u64),
}

/// Expands the position table entries to one [`WavePosition`] for each of the
/// `count` variables.
///
/// The length of each waves entry is the distance to the next one; the last
/// one ends at `waves_end`.
fn resolve_positions(
    entries: Vec<PositionEntry>,
    count: u64,
    waves_end: u64,
) -> std::io::Result<Vec<WavePosition>> {
    let count = to_usize(count)?;
    let mut positions = Vec::with_capacity(count.min(entries.len() * 2));
    // Index in `positions` of the last Data entry, to fill in its length.
    let mut last_data: Option<usize> = None;

    for entry in entries {
        match entry {
            PositionEntry::Zeros(run) => {
                let run = to_usize(run)?;
                if positions.len() + run > count {
                    return Err(invalid_data("position table has too many entries"));
                }
                positions.resize(positions.len() + run, WavePosition::None);
                continue;
            }
            PositionEntry::Offset(offset) => {
                end_last_data(&mut positions, last_data, offset)?;
                last_data = Some(positions.len());
                positions.push(WavePosition::Data { offset, length: 0 });
            }
            PositionEntry::Alias(id) => positions.push(WavePosition::Alias(id)),
        }
        if positions.len() > count {
            return Err(invalid_data("position table has too many entries"));
        }
    }
    end_last_data(&mut positions, last_data, waves_end)?;

    // Trailing variables without changes may be left out.
    positions.resize(count, WavePosition::None);
    Ok(positions)
}

/// Sets the length of the Data entry at `last_data`, which ends at `end`.
fn end_last_data(
    positions: &mut [WavePosition],
    last_data: Option<usize>,
    end: u64,
) -> std::io::Result<()> {
    if let Some(index) = last_data {
        let WavePosition::Data { offset, length } = &mut positions[index] else {
            unreachable!()
        };
        if end <= *offset {
            return Err(invalid_data("wave offsets out of order"));
        }
        *length = end - *offset;
    }
    Ok(())
}

/// The bits array: the value of each variable at the start of the block.
struct BitsArray {
    data: Vec<u8>,
    /// Number of variables.
    count: u64,
}

impl BitsArray {
    /// Returns the value of each variable, given the variable lengths from the
    /// geometry block. Variable length values are not stored, and are returned
    /// as `None`.
    fn values<'a>(
        &'a self,
        lengths: &'a [VariableLength],
    ) -> impl Iterator<Item = std::io::Result<Value<'a>>> + 'a {
        let count = usize::try_from(self.count).unwrap_or(usize::MAX);
        let mut data = &self.data[..];
        lengths.iter().take(count).map(move |length| {
            let size = match *length {
                VariableLength::Bits(bits) => to_usize(bits)?,
                VariableLength::Real => 8,
                VariableLength::VarLen => return Ok(Value::VarLen(&[])),
            };
            if data.len() < size {
                return Err(invalid_data("bits array shorter than geometry"));
            }
            let (value, rest) = data.split_at(size);
            data = rest;
            Ok(match *length {
                VariableLength::Bits(1) => Value::Bit(value[0]),
                VariableLength::Real => Value::Real(value.try_into().unwrap()),
                _ => Value::Ascii(value),
            })
        })
    }
}

/// The value changes of one variable in a block.
struct Waves {
    /// Decompressed waves.
    data: Vec<u8>,
    /// Variable length, from the geometry block.
    length: VariableLength,
}

impl Waves {
    /// Returns the value changes, as `(time_index, value)` pairs. The time
    /// index is an index into the time table.
    fn iter(&self) -> WavesIter<'_> {
        WavesIter {
            data: &self.data,
            length: self.length,
            time_index: 0,
        }
    }
}

struct WavesIter<'a> {
    data: &'a [u8],
    length: VariableLength,
    time_index: u64,
}

/// FST_RCV_STR: values of 1-bit variables other than 0 and 1.
const ONE_BIT_VALUES: &[u8; 8] = b"xzhuwl-?";

impl<'a> WavesIter<'a> {
    fn read_change(&mut self) -> std::io::Result<(u64, Value<'a>)> {
        let header = read_varint(&mut self.data)?;

        let (delta, value) = match self.length {
            VariableLength::Bits(1) => {
                if header & 1 == 0 {
                    (header >> 2, Value::Bit(b'0' | ((header >> 1) & 1) as u8))
                } else {
                    let value = ONE_BIT_VALUES[((header >> 1) & 7) as usize];
                    (header >> 4, Value::Bit(value))
                }
            }
            VariableLength::VarLen => {
                let len = to_usize(read_varint(&mut self.data)?)?;
                (header >> 1, Value::VarLen(self.take(len)?))
            }
            VariableLength::Real => {
                let value = if header & 1 == 0 {
                    // Very rare: every byte of the f64 is b'0' or b'1', so
                    // each one was packed into a bit.
                    let packed = self.take(1)?[0];
                    std::array::from_fn(|i| b'0' | ((packed >> (7 - i)) & 1))
                } else {
                    self.take(8)?.try_into().unwrap()
                };
                (header >> 1, Value::Real(value))
            }
            VariableLength::Bits(bits) => {
                let bits = to_usize(bits)?;
                let value = if header & 1 == 0 {
                    Value::Packed {
                        data: self.take(bits.div_ceil(8))?,
                        bits,
                    }
                } else {
                    Value::Ascii(self.take(bits)?)
                };
                (header >> 1, value)
            }
        };

        self.time_index = checked_add(self.time_index, delta)?;
        Ok((self.time_index, value))
    }

    fn take(&mut self, len: usize) -> std::io::Result<&'a [u8]> {
        if self.data.len() < len {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let (value, rest) = self.data.split_at(len);
        self.data = rest;
        Ok(value)
    }
}

impl<'a> Iterator for WavesIter<'a> {
    type Item = std::io::Result<(u64, Value<'a>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            return None;
        }
        let change = self.read_change();
        if change.is_err() {
            // Don't keep reading after an error.
            self.data = &[];
        }
        Some(change)
    }
}

/// A variable value, as encoded in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value<'a> {
    /// A 1-bit value, as ASCII: b'0', b'1', b'x', b'z', b'h', b'u', b'w', b'l',
    /// b'-' or b'?'.
    Bit(u8),
    /// A value with only 0s and 1s, packed into bytes, most significant bit
    /// first.
    Packed { data: &'a [u8], bits: usize },
    /// A value with one ASCII character per bit.
    Ascii(&'a [u8]),
    /// A real, as the raw bytes of an f64 (see [`HeaderBlock::real_endianness`]).
    Real([u8; 8]),
    /// A variable length value.
    VarLen(&'a [u8]),
}

impl Value<'_> {
    /// Returns the value with one ASCII character per bit. For reals and
    /// variable length values, returns the raw bytes.
    fn to_ascii(&self) -> Vec<u8> {
        match *self {
            Value::Bit(bit) => vec![bit],
            Value::Packed { data, bits } => (0..bits)
                .map(|i| b'0' | ((data[i / 8] >> (7 - i % 8)) & 1))
                .collect(),
            Value::Ascii(data) | Value::VarLen(data) => data.to_vec(),
            Value::Real(bytes) => bytes.to_vec(),
        }
    }

    /// Returns a value that can be displayed, decoding reals with the given
    /// endianness.
    fn display(&self, endianness: RealEndianess) -> String {
        match *self {
            Value::Real(bytes) => format!("{}", real_to_f64(bytes, endianness)),
            _ => String::from_utf8_lossy(&self.to_ascii()).into_owned(),
        }
    }
}

fn real_to_f64(bytes: [u8; 8], endianness: RealEndianess) -> f64 {
    match endianness {
        RealEndianess::LittleEndian => f64::from_le_bytes(bytes),
        RealEndianess::BigEndian => f64::from_be_bytes(bytes),
    }
}

/// Decompresses zlib data whose decompressed size must be `len`.
fn zlib_decompress(input: &[u8], len: u64) -> std::io::Result<Vec<u8>> {
    let len = to_usize(len)?;
    let mut output = Vec::with_capacity(len);
    ZlibDecoder::new(input)
        .take(len as u64 + 1)
        .read_to_end(&mut output)?;
    if output.len() != len {
        return Err(invalid_data("zlib data has unexpected length"));
    }
    Ok(output)
}

/// Reads a signed LEB128 varint.
fn read_svarint<R: Read>(reader: &mut R) -> std::io::Result<i64> {
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = read_u8(reader)?;
        if shift >= 64 {
            return Err(invalid_data("svarint overflows i64"));
        }
        value |= u64::from(byte & 0x7F) << shift;
        shift += 7;

        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                // Sign extend.
                value |= !0 << shift;
            }
            return Ok(value as i64);
        }
    }
}

fn checked_add(a: u64, b: u64) -> std::io::Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| invalid_data("offset overflows u64"))
}

fn to_usize(value: u64) -> std::io::Result<usize> {
    usize::try_from(value).map_err(|_| invalid_data("value too large"))
}

fn invalid_data(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Skips the bytes left in `reader` up to its limit.
fn skip_remaining<R: Seek>(reader: &mut Take<R>) -> std::io::Result<()> {
    let remaining = reader.limit();
    if remaining > 0 {
        let offset = i64::try_from(remaining).map_err(|_| invalid_data("section too large"))?;
        reader.get_mut().seek_relative(offset)?;
        reader.set_limit(0);
    }
    Ok(())
}

struct SectionIterator<R: Read + Seek> {
    /// Limited to the bytes left unread in the last returned section.
    reader: Take<R>,
}

impl<R: Read + Seek> SectionIterator<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: reader.take(0),
        }
    }

    /// Returns the next section, or `None` at end of file.
    ///
    /// Any data left unread in the previous section is skipped first.
    fn next(&mut self) -> std::io::Result<Option<Section<'_, R>>> {
        skip_remaining(&mut self.reader)?;
        let reader = self.reader.get_mut();

        let mut kind: u8 = 0;
        match reader.read_exact(std::slice::from_mut(&mut kind)) {
            Ok(_) => (),
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        }

        let mut len: [u8; 8] = [0; 8];
        reader.read_exact(&mut len)?;

        // The section length includes the length field itself.
        let len = u64::from_be_bytes(len)
            .checked_sub(8)
            .ok_or_else(|| invalid_data("section length smaller than 8"))?;

        self.reader.set_limit(len);
        Ok(Some(Section {
            block_type: kind,
            len,
            data: &mut self.reader,
        }))
    }
}

#[cfg(test)]
mod tests {
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
    fn hierarchy_section(
        block_type: BlockType,
        uncompressed_length: usize,
        data: &[u8],
    ) -> Vec<u8> {
        let mut bytes = (uncompressed_length as u64).to_be_bytes().to_vec();
        bytes.extend_from_slice(data);
        let mut file = section(block_type as u8, &bytes);
        file.extend(section(BlockType::Blackout as u8, &[]));
        file
    }

    fn read_hierarchy_entries(
        file: Vec<u8>,
        compression: HierarchyCompression,
    ) -> Vec<HierarchyEntry> {
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
        assert_eq!(block.waves_count, 4);
        assert_eq!(block.pack_type, b'Z');

        // Read the tables out of order, to exercise seeking.
        assert_eq!(block.read_time_table().unwrap(), [10, 15, 22]);

        let changes = |block: &mut ValueChangeBlock<_>, id: u64| -> Vec<(u64, Vec<u8>)> {
            let waves = block.waves(id, lengths[id as usize]).unwrap();
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

        let bits = block.read_bits().unwrap();
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

        assert_eq!(block.positions().unwrap()[2], WavePosition::Alias(1));

        let next = iterator.next().unwrap().unwrap();
        assert_eq!(next.block_type(), Some(BlockType::Blackout));
    }

    #[test]
    fn value_change_block_corrupt_trailer() {
        let mut file = value_change_block();
        // The blackout section is 9 bytes, the time count is the last field.
        let trailer = file.len() - 9 - 24;
        // Time table compressed length larger than the block.
        file[trailer + 8..trailer + 16].copy_from_slice(&u64::MAX.to_be_bytes());
        let mut iterator = SectionIterator::new(std::io::Cursor::new(file));
        assert!(
            iterator
                .next()
                .unwrap()
                .unwrap()
                .read_value_changes()
                .is_err()
        );
    }
}
