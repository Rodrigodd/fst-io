use std::io::{BufRead, Cursor, Read, Seek, Take, Write};

use flate2::read::{GzDecoder, ZlibDecoder};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockType {
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
pub struct Section<R> {
    pub block_type: u8,
    pub len: u64,
    /// The iterator's reader, limited to the remaining bytes of this section.
    pub data: R,
}

impl<R: Read> Section<R> {
    /// Reads the whole section into memory and returns an owned, seekable
    /// section backed by that buffer.
    ///
    /// Sections borrowed from a [`SectionIterator`] seek through the underlying
    /// file; buffering detaches a section from the iterator so it can be held
    /// and read independently (for example, decoded on a worker while the
    /// iterator moves on). The random-access `read_*` methods work on either
    /// form, since both the borrowed reader and the buffer implement
    /// [`SectionData`].
    pub fn buffer(self) -> std::io::Result<Section<Cursor<Vec<u8>>>> {
        let mut data = Vec::with_capacity(to_usize(self.len).unwrap_or(0));
        let read = self.data.take(self.len).read_to_end(&mut data)?;
        if read as u64 != self.len {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(Section {
            block_type: self.block_type,
            len: self.len,
            data: Cursor::new(data),
        })
    }
}

/// A reader positioned within a single section, able to report and change its
/// offset relative to the start of the section data.
///
/// This is what lets the `read_*` methods be generic over how a section is
/// backed. The borrowed form (`&mut Take<R>`, handed out by
/// [`SectionIterator`]) seeks the underlying file and keeps the `Take` limit in
/// step; the owned form ([`Cursor<Vec<u8>>`], from [`Section::buffer`]) seeks
/// its in-memory buffer directly.
///
/// `section_len` is passed in rather than stored because the borrowed form only
/// knows how many bytes are *left* (the `Take` limit), not where the section
/// began; the owning `ValueChangeBlock`/`Section` always knows its own length.
pub trait SectionData: Read {
    /// Current offset from the start of the section data.
    fn section_position(&self, section_len: u64) -> u64;
    /// Seeks to `position`, an offset from the start of the section data.
    fn section_seek_to(&mut self, position: u64, section_len: u64) -> std::io::Result<()>;
}

impl<R: Read + Seek> SectionData for &mut Take<R> {
    fn section_position(&self, section_len: u64) -> u64 {
        // `set_limit(section_len)` is established when the section begins and
        // every read decrements the limit, so the consumed prefix is the
        // difference.
        section_len - self.limit()
    }

    fn section_seek_to(&mut self, position: u64, section_len: u64) -> std::io::Result<()> {
        let current = section_len - self.limit();
        let offset = i64::try_from(position).map_err(|_| invalid_data("section too large"))?
            - i64::try_from(current).map_err(|_| invalid_data("section too large"))?;
        self.get_mut().seek_relative(offset)?;
        self.set_limit(section_len - position);
        Ok(())
    }
}

impl SectionData for Cursor<Vec<u8>> {
    fn section_position(&self, _section_len: u64) -> u64 {
        self.position()
    }

    fn section_seek_to(&mut self, position: u64, _section_len: u64) -> std::io::Result<()> {
        self.set_position(position);
        Ok(())
    }
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

impl HeaderBlock {
    /// Length of the header block data.
    pub const LEN: usize = 321;

    /// Encodes the header block data, the inverse of [`Section::read_header`].
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut data = [0u8; Self::LEN];
        data[0..8].copy_from_slice(&self.start_time.to_be_bytes());
        data[8..16].copy_from_slice(&self.end_time.to_be_bytes());
        data[16..24].copy_from_slice(&match self.real_endianness {
            RealEndianess::LittleEndian => std::f64::consts::E.to_le_bytes(),
            RealEndianess::BigEndian => std::f64::consts::E.to_be_bytes(),
        });
        data[24..32].copy_from_slice(&self.writer_memory_use.to_be_bytes());
        data[32..40].copy_from_slice(&self.num_scopes.to_be_bytes());
        data[40..48].copy_from_slice(&self.num_hierarchy_vars.to_be_bytes());
        data[48..56].copy_from_slice(&self.num_vars.to_be_bytes());
        data[56..64].copy_from_slice(&self.num_vc_blocks.to_be_bytes());
        data[64] = self.timescale as u8;
        data[65..193].copy_from_slice(&self.writer);
        data[193..219].copy_from_slice(&self.date);
        data[219..312].copy_from_slice(&self.reserved);
        data[312] = self.file_type;
        data[313..321].copy_from_slice(&self.timezero.to_be_bytes());
        data
    }
}

impl<R: Read> Read for Section<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.data.read(buf)
    }
}

impl<R> Section<R> {
    /// Length of the section data, in bytes (excluding block type and length fields).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn block_type(&self) -> Option<BlockType> {
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

impl<R: Read> Section<R> {
    /// Writes the whole section, including the block type and length, to `out`.
    ///
    /// The section data must not have been read yet.
    pub fn copy_to<W: Write>(&mut self, out: &mut W) -> std::io::Result<()> {
        write_section_header(out, self.block_type, self.len)?;
        let copied = std::io::copy(&mut self.data, out)?;
        if copied != self.len {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}

/// Writes the block type and length fields that start a section.
///
/// `len` is the length of the section data.
pub fn write_section_header<W: Write>(
    out: &mut W,
    block_type: u8,
    len: u64,
) -> std::io::Result<()> {
    out.write_all(&[block_type])?;
    // The section length includes the length field itself.
    out.write_all(&(len + 8).to_be_bytes())
}

impl<R: Read + Seek> Section<R> {
    /// Reads the section as a header block.
    pub fn read_header(mut self) -> std::io::Result<HeaderBlock> {
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

impl<R: Read> Section<R> {
    /// Reads the section as a geometry block.
    ///
    /// Only the fixed size fields are read; the length entries are read lazily
    /// through [`GeometryBlock::lengths`].
    pub fn read_geometry(mut self) -> std::io::Result<GeometryBlock<R>> {
        if self.block_type != BlockType::Geometry as u8 {
            return Err(invalid_data("section is not a geometry block"));
        }

        let mut fields = [0u8; 16];
        self.read_exact(&mut fields)?;

        let uncompressed_length = u64::from_be_bytes(fields[0..8].try_into().unwrap());
        let count = u64::from_be_bytes(fields[8..16].try_into().unwrap());

        // The data that follows the 16 fixed-size fields is the (possibly
        // compressed) length table, read sequentially to the end of the
        // section.
        let compressed_length = self
            .len
            .checked_sub(16)
            .ok_or_else(|| invalid_data("geometry block too short"))?;

        Ok(GeometryBlock {
            uncompressed_length,
            compressed_length,
            count,
            data: self.data,
        })
    }
}

/// Geometry block: the length of each variable in bits.
pub struct GeometryBlock<R> {
    /// Length of the uncompressed data. Equal to `compressed_length` if the data
    /// is not compressed.
    pub uncompressed_length: u64,
    /// Length of the (possibly compressed) data.
    pub compressed_length: u64,
    /// Number of length entries in the data.
    pub count: u64,
    /// The section reader, positioned at the start of the length table.
    pub data: R,
}

impl<R: Read> GeometryBlock<R> {
    pub fn is_compressed(&self) -> bool {
        self.uncompressed_length != self.compressed_length
    }

    /// Returns an iterator over the length of each variable.
    pub fn lengths(self) -> GeometryLengths<GeometryData<R>> {
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
pub enum VariableLength {
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
pub enum GeometryData<R> {
    Raw(R),
    Zlib(ZlibDecoder<R>),
}

impl<R: Read> Read for GeometryData<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            GeometryData::Raw(data) => data.read(buf),
            GeometryData::Zlib(data) => data.read(buf),
        }
    }
}

pub struct GeometryLengths<R> {
    pub data: R,
    /// Entries not yet read.
    pub remaining: u64,
}

impl<R: Read> Iterator for GeometryLengths<R> {
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

impl<R: Read> Section<R> {
    /// Reads the section as a hierarchy block.
    ///
    /// Only the fixed size fields are read; the entries are decompressed and
    /// read by [`HierarchyBlock::entries`].
    pub fn read_hierarchy(mut self) -> std::io::Result<HierarchyBlock<R>> {
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
pub enum HierarchyCompression {
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
pub struct HierarchyBlock<R> {
    pub compression: HierarchyCompression,
    /// Length of the hierarchy data after decompression.
    pub uncompressed_length: u64,
    /// The section reader, positioned at the start of the compressed data.
    pub data: R,
}

impl<R: Read> HierarchyBlock<R> {
    /// Returns an iterator over the hierarchy entries.
    ///
    /// Gzip data is decompressed while iterating. LZ4 data can only be
    /// decompressed as a whole, so it is read and decompressed here.
    pub fn entries(self) -> std::io::Result<HierarchyEntries<HierarchyData<R>>> {
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

/// Reads the rest of `reader` to the end of the section.
///
/// Both section readers stop at the section boundary on their own -- the
/// borrowed `Take<R>` at its limit, the buffered `Cursor` at the end of its
/// buffer -- so a plain `read_to_end` captures exactly the section data.
fn read_to_vec<R: Read>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
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
pub enum HierarchyData<R> {
    Gzip(std::io::BufReader<GzDecoder<R>>),
    Buffer(std::io::Cursor<Vec<u8>>),
}

impl<R: Read> Read for HierarchyData<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            HierarchyData::Gzip(data) => data.read(buf),
            HierarchyData::Buffer(data) => data.read(buf),
        }
    }
}

impl<R: Read> BufRead for HierarchyData<R> {
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
pub enum HierarchyEntry {
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

pub struct HierarchyEntries<R> {
    pub data: R,
    /// ID of the next non-aliased variable.
    pub next_id: u64,
    /// Set at the end of the data or after an error.
    pub done: bool,
}

impl<R: BufRead> HierarchyEntries<R> {
    pub fn read_entry(&mut self) -> std::io::Result<Option<HierarchyEntry>> {
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

impl<R: BufRead> Iterator for HierarchyEntries<R> {
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

impl<R: SectionData> Section<R> {
    /// Reads the section as a value change block.
    ///
    /// Reads only the fixed size fields at the start of the section. The
    /// tables are located by [`ValueChangeBlock::read_head_layout`] and
    /// [`ValueChangeBlock::read_tail_layout`], and read on demand.
    pub fn read_value_changes(mut self) -> std::io::Result<ValueChangeBlock<R>> {
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

        Ok(ValueChangeBlock {
            block_type,
            start_time,
            end_time,
            memory_required,
            len: self.len,
            data: self.data,
        })
    }
}

/// Value change block: the value of every variable over a period of time.
///
/// Contains four tables, read on demand: the bits array (values at
/// `start_time`), the waves (the value changes of each variable), the position
/// table (where the waves of each variable are) and the time table.
///
/// The block holds no derived state. The caller locates the tables with
/// [`HeadLayout`] and [`TailLayout`], and passes them to the methods that read
/// the tables.
pub struct ValueChangeBlock<R> {
    pub block_type: BlockType,
    /// Start time of the block, in units of the header timescale.
    pub start_time: u64,
    pub end_time: u64,
    /// Buffer memory required for a full traversal of the block.
    pub memory_required: u64,

    /// Length of the section data.
    pub len: u64,
    /// The section reader.
    pub data: R,
}

/// Fields of a value change block located by seeking from the start of the
/// section. Offsets are relative to the start of the section data.
#[derive(Debug, Clone, Copy)]
pub struct HeadLayout {
    pub bits_start: u64,
    pub bits_uncompressed_length: u64,
    /// Equal to `bits_uncompressed_length` if not compressed.
    pub bits_compressed_length: u64,
    /// Number of variables in the bits array.
    pub bits_count: u64,
    /// Number of variables in the position table.
    pub waves_count: u64,
    /// Offset of the pack type byte. Wave positions are relative to it.
    pub waves_start: u64,
    /// Compression of the waves: b'Z' (zlib), b'F' (FastLZ) or b'4' (LZ4).
    pub pack_type: u8,
}

/// Fields of a value change block located by seeking from the end of the
/// section. Offsets are relative to the start of the section data.
#[derive(Debug, Clone, Copy)]
pub struct TailLayout {
    pub positions_start: u64,
    pub time_start: u64,
    pub time_uncompressed_length: u64,
    /// Equal to `time_uncompressed_length` if not compressed.
    pub time_compressed_length: u64,
    /// Number of entries in the time table.
    pub time_count: u64,
}

/// Where the waves of a variable are, as decoded from the position table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavePosition {
    /// The variable has no value changes in this block.
    None,
    /// The waves are at `offset` from the pack type byte, and are `length`
    /// bytes long.
    Data { offset: u64, length: u64 },
    /// Dynamic alias: the variable has the same waves as variable `id`.
    Alias(u64),
}

impl<R: SectionData> ValueChangeBlock<R> {
    /// Current position, relative to the start of the section data.
    fn position(&self) -> u64 {
        self.data.section_position(self.len)
    }

    /// Seeks to `position`, relative to the start of the section data.
    fn seek_to(&mut self, position: u64) -> std::io::Result<()> {
        if position > self.len {
            return Err(invalid_data("offset past end of value change block"));
        }
        self.data.section_seek_to(position, self.len)
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

    /// Writes the whole section, including the block type and length, to
    /// `out`.
    pub fn copy_to<W: Write>(&mut self, out: &mut W) -> std::io::Result<()> {
        self.seek_to(0)?;
        write_section_header(out, self.block_type as u8, self.len)?;
        let copied = std::io::copy(&mut self.data, out)?;
        if copied != self.len {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }

    /// Locates the tables at the start of the section: the bits array and the
    /// waves.
    pub fn read_head_layout(&mut self) -> std::io::Result<HeadLayout> {
        // The fixed size fields, read by `read_value_changes`, come first.
        self.seek_to(24)?;
        let bits_uncompressed_length = read_varint(&mut self.data)?;
        let bits_compressed_length = read_varint(&mut self.data)?;
        let bits_count = read_varint(&mut self.data)?;

        let bits_start = self.position();
        self.seek_to(checked_add(bits_start, bits_compressed_length)?)?;
        let waves_count = read_varint(&mut self.data)?;
        let waves_start = self.position();
        let pack_type = read_u8(&mut self.data)?;

        Ok(HeadLayout {
            bits_start,
            bits_uncompressed_length,
            bits_compressed_length,
            bits_count,
            waves_count,
            waves_start,
            pack_type,
        })
    }

    /// Locates the tables at the end of the section: the position table and
    /// the time table.
    pub fn read_tail_layout(&mut self) -> std::io::Result<TailLayout> {
        // The time table trailer is at the end of the section.
        let trailer_start = self
            .len
            .checked_sub(24)
            .ok_or_else(|| invalid_data("value change block too short"))?;
        self.seek_to(trailer_start)?;
        let mut trailer = [0u8; 24];
        self.data.read_exact(&mut trailer)?;
        let time_uncompressed_length = u64::from_be_bytes(trailer[0..8].try_into().unwrap());
        let time_compressed_length = u64::from_be_bytes(trailer[8..16].try_into().unwrap());
        let time_count = u64::from_be_bytes(trailer[16..24].try_into().unwrap());

        // The position table length is just before the time table.
        let time_start = trailer_start
            .checked_sub(time_compressed_length)
            .ok_or_else(|| invalid_data("time table larger than block"))?;
        let positions_length_start = time_start
            .checked_sub(8)
            .ok_or_else(|| invalid_data("time table larger than block"))?;
        self.seek_to(positions_length_start)?;
        let mut positions_length = [0u8; 8];
        self.data.read_exact(&mut positions_length)?;
        let positions_length = u64::from_be_bytes(positions_length);

        let positions_start = positions_length_start
            .checked_sub(positions_length)
            .ok_or_else(|| invalid_data("position table larger than block"))?;

        Ok(TailLayout {
            positions_start,
            time_start,
            time_uncompressed_length,
            time_compressed_length,
            time_count,
        })
    }

    /// Reads the bits array: the value of each variable at `start_time`.
    pub fn read_bits(&mut self, head: &HeadLayout) -> std::io::Result<BitsArray> {
        let data = self.read_at(head.bits_start, head.bits_compressed_length)?;
        let data = if head.bits_compressed_length != head.bits_uncompressed_length {
            zlib_decompress(&data, head.bits_uncompressed_length)?
        } else {
            data
        };
        Ok(BitsArray {
            data,
            count: head.bits_count,
        })
    }

    /// Reads the time table, returning the time of each time index.
    pub fn read_time_table(&mut self, tail: &TailLayout) -> std::io::Result<Vec<u64>> {
        let data = self.read_at(tail.time_start, tail.time_compressed_length)?;
        let data = if tail.time_compressed_length != tail.time_uncompressed_length {
            zlib_decompress(&data, tail.time_uncompressed_length)?
        } else {
            data
        };

        let mut reader = &data[..];
        let mut time = 0u64;
        let mut times = Vec::with_capacity(to_usize(tail.time_count)?.min(data.len()));
        for _ in 0..tail.time_count {
            time = time
                .checked_add(read_varint(&mut reader)?)
                .ok_or_else(|| invalid_data("time overflows u64"))?;
            times.push(time);
        }
        Ok(times)
    }

    /// Reads the position table.
    ///
    /// Has `waves_count` entries, one for each variable.
    pub fn read_positions(
        &mut self,
        head: &HeadLayout,
        tail: &TailLayout,
    ) -> std::io::Result<Vec<WavePosition>> {
        if tail.positions_start <= head.waves_start {
            return Err(invalid_data("position table larger than block"));
        }
        let length = tail.time_start - 8 - tail.positions_start;
        let data = self.read_at(tail.positions_start, length)?;
        let waves_end = tail.positions_start - head.waves_start;
        match self.block_type {
            BlockType::ValueChangeDataDynamicAlias2 => {
                decode_positions_alias2(&data, head.waves_count, waves_end)
            }
            _ => decode_positions(&data, head.waves_count, waves_end),
        }
    }

    /// Reads and decompresses the waves of variable `id`, following dynamic
    /// aliases.
    ///
    /// `positions` is the table from [`Self::read_positions`]. `length` is the
    /// variable length from the geometry block, needed to decode the waves.
    #[inline(never)]
    pub fn read_waves(
        &mut self,
        head: &HeadLayout,
        positions: &[WavePosition],
        id: u64,
        length: VariableLength,
    ) -> std::io::Result<Waves> {
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
                let raw = self.read_at(head.waves_start + offset, length)?;
                let mut reader = &raw[..];
                let uncompressed_length = read_varint(&mut reader)?;
                if uncompressed_length == 0 {
                    reader.to_vec()
                } else {
                    match head.pack_type {
                        b'4' => lz4_decompress(reader, uncompressed_length)?,
                        b'F' => fastlz_decompress(reader, uncompressed_length)?,
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
            let alias = if value == 0 {
                // 0 repeats the previous alias.
                previous_alias
                    .ok_or_else(|| invalid_data("repeated alias without a previous one"))?
            } else {
                previous_alias = Some(value);
                value
            };
            // The alias is -(id + 1).
            entries.push(PositionEntry::Alias((-(alias + 1)) as u64));
        }
    }
    resolve_positions(entries, count, waves_end)
}

/// A decoded position table entry, before the wave lengths are known.
pub enum PositionEntry {
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
pub struct BitsArray {
    pub data: Vec<u8>,
    /// Number of variables.
    pub count: u64,
}

impl BitsArray {
    /// Returns the value of each variable, given the variable lengths from the
    /// geometry block. Variable length values are not stored, and are returned
    /// as `None`.
    pub fn values<'a>(
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
pub struct Waves {
    /// Decompressed waves.
    pub data: Vec<u8>,
    /// Variable length, from the geometry block.
    pub length: VariableLength,
}

impl Waves {
    /// Returns the value changes, as `(time_index, value)` pairs. The time
    /// index is an index into the time table.
    pub fn iter(&self) -> WavesIter<'_> {
        WavesIter {
            data: &self.data,
            length: self.length,
            time_index: 0,
        }
    }
}

pub struct WavesIter<'a> {
    pub data: &'a [u8],
    pub length: VariableLength,
    pub time_index: u64,
}

/// FST_RCV_STR: values of 1-bit variables other than 0 and 1.
const ONE_BIT_VALUES: &[u8; 8] = b"xzhuwl-?";

impl<'a> WavesIter<'a> {
    pub fn read_change(&mut self) -> std::io::Result<(u64, Value<'a>)> {
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

    pub fn take(&mut self, len: usize) -> std::io::Result<&'a [u8]> {
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
pub enum Value<'a> {
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
    pub fn to_ascii(&self) -> Vec<u8> {
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
    pub fn display(&self, endianness: RealEndianess) -> String {
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

/// Decompresses FastLZ data whose decompressed size must be `len`.
fn fastlz_decompress(input: &[u8], len: u64) -> std::io::Result<Vec<u8>> {
    let len = to_usize(len)?;
    let mut output = vec![0u8; len];
    let written = fastlz_rs::decompress_to_buf(input, &mut output)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if written != len {
        return Err(invalid_data("FastLZ data shorter than expected"));
    }
    Ok(output)
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

pub struct SectionIterator<R: Read + Seek> {
    /// Limited to the bytes left unread in the last returned section.
    pub reader: R,
}

impl<R: Read + Seek> SectionIterator<Take<R>> {
    pub fn new(reader: R) -> SectionIterator<Take<R>> {
        Self {
            reader: reader.take(0),
        }
    }
}

impl<R: Read + Seek> SectionIterator<Take<R>> {
    /// Returns the next section, or `None` at end of file.
    ///
    /// Any data left unread in the previous section is skipped first.
    pub fn next(&mut self) -> std::io::Result<Option<Section<&mut Take<R>>>> {
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
