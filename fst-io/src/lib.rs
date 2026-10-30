use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Take, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        self.buffer_reusing(Vec::new())
    }

    /// Like [`Section::buffer`], but reads into a caller-provided `Vec`,
    /// reusing its capacity. The buffer is cleared first, so any previous
    /// contents are discarded. Use this to recycle one allocation across many
    /// sections instead of allocating a fresh buffer per section.
    pub fn buffer_reusing(self, mut buf: Vec<u8>) -> std::io::Result<Section<Cursor<Vec<u8>>>> {
        buf.clear();
        buf.reserve(to_usize(self.len).unwrap_or(0));
        let read = self.data.take(self.len).read_to_end(&mut buf)?;
        if read as u64 != self.len {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(Section {
            block_type: self.block_type,
            len: self.len,
            data: Cursor::new(buf),
        })
    }
}

impl Section<Cursor<Vec<u8>>> {
    /// The start time recorded in a buffered value-change block's header: the
    /// first 8 bytes of the section data, before the other fixed-size fields.
    pub fn value_change_start_time(&self) -> std::io::Result<u64> {
        let data = self.data.get_ref();
        let bytes = data
            .get(0..8)
            .ok_or_else(|| invalid_data("value change block too short"))?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// The first entry of a buffered value-change block's time table, or `None`
    /// if the block has no time steps.
    pub fn value_change_first_time(&self) -> std::io::Result<Option<u64>> {
        // Reading the time table consumes the section, so work on a clone and
        // leave this one intact for the caller.
        let clone = Section {
            block_type: self.block_type,
            len: self.len,
            data: Cursor::new(self.data.get_ref().clone()),
        };
        let mut block = clone.read_value_changes()?;
        let tail = block.read_tail_layout()?;
        let times = block.read_time_table(&tail)?;
        Ok(times.first().copied())
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

    /// Whether the section carries no data beyond its block type and length.
    pub fn is_empty(&self) -> bool {
        self.len == 0
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

impl VariableLength {
    /// The geometry block entry, the inverse of [`GeometryLengths`].
    pub fn to_geometry(self) -> u64 {
        match self {
            VariableLength::Bits(bits) => bits,
            VariableLength::Real => 0,
            VariableLength::VarLen => 0xFFFF_FFFF,
        }
    }
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

/// Appends `value` as an unsigned LEB128 varint, the inverse of [`read_varint`].
fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
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
/// FST_VT_VCD_REAL: a 64-bit real.
pub const VAR_TYPE_REAL: u8 = 3;
/// FST_VT_VCD_REAL_PARAMETER: a 64-bit real.
pub const VAR_TYPE_REAL_PARAMETER: u8 = 4;
/// FST_VT_VCD_PORT
pub const VAR_TYPE_PORT: u8 = 18;
/// FST_VT_VCD_REALTIME: a 64-bit real.
pub const VAR_TYPE_REALTIME: u8 = 20;
/// FST_VT_SV_SHORTREAL: a 32-bit real.
pub const VAR_TYPE_SHORTREAL: u8 = 29;

/// Variable types the geometry block records as a real (`VariableLength::Real`):
/// the three 64-bit reals and the 32-bit shortreal.
fn is_real_var_type(var_type: u8) -> bool {
    matches!(
        var_type,
        VAR_TYPE_REAL | VAR_TYPE_REAL_PARAMETER | VAR_TYPE_REALTIME | VAR_TYPE_SHORTREAL
    )
}

/// Derives the per-signal geometry lengths from hierarchy entries.
///
/// A dump interrupted before its geometry section was written has no geometry
/// block, but the geometry is redundant: it is reconstructed from the variable
/// records, one entry per non-aliased variable in id order, exactly as the
/// geometry block would have stored them. A real is recorded as
/// [`VariableLength::Real`], a zero-length variable (such as a string) as
/// [`VariableLength::VarLen`], and everything else by its bit length.
pub fn geometry_from_entries<I>(entries: I) -> std::io::Result<Vec<VariableLength>>
where
    I: IntoIterator<Item = std::io::Result<HierarchyEntry>>,
{
    let mut lengths = Vec::new();
    for entry in entries {
        if let HierarchyEntry::Var {
            var_type,
            length,
            is_alias,
            ..
        } = entry?
        {
            if is_alias {
                continue;
            }
            let length = if is_real_var_type(var_type) {
                VariableLength::Real
            } else if length == 0 {
                VariableLength::VarLen
            } else {
                VariableLength::Bits(length)
            };
            lengths.push(length);
        }
    }
    Ok(lengths)
}

/// A dense, 1-based signal handle, the identity of a variable across the
/// high-level reader API.
///
/// Wraps a [`NonZeroU32`] so an id and a 0-based geometry index cannot be
/// confused. [`index`](Self::index) converts to the 0-based geometry index the
/// block-level readers and geometry table use; [`from_index`](Self::from_index)
/// is the inverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FstSignalHandle(NonZeroU32);

impl FstSignalHandle {
    /// Builds a handle from a 0-based geometry index.
    pub fn from_index(index: usize) -> FstSignalHandle {
        FstSignalHandle(NonZeroU32::new(index as u32 + 1).expect("index + 1 is non-zero"))
    }

    /// The 0-based geometry index this handle refers to.
    pub fn index(&self) -> usize {
        (self.0.get() - 1) as usize
    }
}

impl std::fmt::Display for FstSignalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "H{}", self.0.get())
    }
}

/// An inclusive time window, `[start, end]`, for
/// [`FstReader::read_value_changes`].
#[derive(Debug, Clone, Copy)]
pub struct TimeFilter {
    pub start: u64,
    pub end: u64,
}

/// A high-level reader that ties a whole FST file together: its header,
/// geometry, hierarchy, and value changes.
///
/// It owns an [`FstInput`] (which reopens a fresh seekable reader per pass and
/// decompresses a whole-file gzip wrapper once) and the optional sibling
/// hierarchy path a simulator writes for an in-progress dump. The header and
/// geometry are read on [`open`](Self::open); the hierarchy and value changes
/// are streamed on demand. The reader reopens the input internally, so callers
/// no longer manage the file's multiple passes themselves.
///
/// This sits on top of [`SectionIterator`] and the block readers; it adds no new
/// parsing. The parallel/clocked consumers that need every cycle of performance
/// keep driving the block readers directly.
pub struct FstReader {
    input: FstInput,
    hier_sibling: Option<PathBuf>,
    header: HeaderBlock,
    geometry: Vec<VariableLength>,
}

impl FstReader {
    /// Opens `input`, reading the header and geometry.
    pub fn open(input: FstInput) -> std::io::Result<FstReader> {
        Self::open_with_hierarchy(input, None)
    }

    /// Opens `input`, preferring `hier_sibling` (a `<file>.fst.hier` file a
    /// simulator writes for an in-progress dump) over any embedded hierarchy
    /// block when reading the hierarchy.
    ///
    /// The sibling is also what reconstructs the geometry when the dump was
    /// interrupted before its geometry block was written.
    pub fn open_with_hierarchy(
        input: FstInput,
        hier_sibling: Option<PathBuf>,
    ) -> std::io::Result<FstReader> {
        let mut header = None;
        let mut geometry = None;
        let mut iterator = SectionIterator::new(input.open()?);
        while let Some(section) = iterator.next()? {
            match section.block_type() {
                Some(BlockType::Header) => header = Some(section.read_header()?),
                Some(BlockType::Geometry) => {
                    geometry = Some(
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

        let mut reader = FstReader {
            input,
            hier_sibling,
            header,
            geometry: Vec::new(),
        };

        // An interrupted dump has no geometry section; reconstruct the lengths
        // from the hierarchy records, which carry the same information.
        reader.geometry = match geometry {
            Some(geometry) => geometry,
            None => {
                let mut entries = Vec::new();
                reader.read_hierarchy(|entry, _| {
                    entries.push(entry.clone());
                    Ok(())
                })?;
                geometry_from_entries(entries.into_iter().map(Ok))?
            }
        };

        Ok(reader)
    }

    /// The file header block.
    pub fn header(&self) -> &HeaderBlock {
        &self.header
    }

    /// The input this reader draws from, for callers still driving the
    /// block-level readers directly.
    pub fn input(&self) -> &FstInput {
        &self.input
    }

    /// The per-variable geometry lengths, indexed by 0-based geometry index
    /// (equivalently, by [`FstSignalHandle::index`]). Reconstructed from the
    /// hierarchy when the file has no geometry block.
    pub fn geometry(&self) -> &[VariableLength] {
        &self.geometry
    }

    /// Calls `callback` for each hierarchy entry, with the names of the
    /// enclosing scopes (innermost last) so a caller need not maintain its own
    /// scope stack to build a variable's dotted path.
    ///
    /// A sibling `<file>.fst.hier` file, when given at open time, is read in
    /// preference to any embedded hierarchy block, matching the reference
    /// library's handling of an in-progress dump.
    pub fn read_hierarchy(
        &self,
        mut callback: impl FnMut(&HierarchyEntry, &[Vec<u8>]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut scopes: Vec<Vec<u8>> = Vec::new();

        if let Some(path) = &self.hier_sibling {
            let reader = BufReader::new(File::open(path)?);
            for entry in HierarchyEntries::from_records(reader) {
                visit_hierarchy_entry(&entry?, &mut scopes, &mut callback)?;
            }
            return Ok(());
        }

        let mut iterator = SectionIterator::new(self.input.open()?);
        while let Some(section) = iterator.next()? {
            if !matches!(
                section.block_type(),
                Some(BlockType::Hierarchy | BlockType::HierarchyLZ4 | BlockType::HierarchyLZ4Duo)
            ) {
                continue;
            }
            for entry in section.read_hierarchy()?.entries()? {
                visit_hierarchy_entry(&entry?, &mut scopes, &mut callback)?;
            }
            return Ok(());
        }
        Err(invalid_data("file has no hierarchy block"))
    }

    /// Streams the value changes of the file as `(time, handle, value)`,
    /// restricted to the inclusive time window and, when `include` is given, to
    /// the listed signals.
    ///
    /// This is the convenience streaming path, modelled on the reference
    /// readers' iterate-blocks callback. It decodes blocks serially on the
    /// calling thread; a consumer that needs the parallel/arena optimizations
    /// drives the block readers directly instead.
    ///
    /// The first argument of `callback` is the absolute time (resolved from the
    /// block's time table). A callback error aborts the stream and is returned
    /// verbatim.
    pub fn read_value_changes<E>(
        &self,
        time: TimeFilter,
        include: Option<&[FstSignalHandle]>,
        mut callback: impl FnMut(u64, FstSignalHandle, Value) -> Result<(), E>,
    ) -> Result<(), ReadError<E>> {
        let mut mask = vec![include.is_none(); self.geometry.len()];
        if let Some(include) = include {
            for handle in include {
                if let Some(slot) = mask.get_mut(handle.index()) {
                    *slot = true;
                }
            }
        }

        let mut iterator = SectionIterator::new(self.input.open().map_err(ReadError::Io)?);
        while let Some(section) = iterator.next().map_err(ReadError::Io)? {
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
            let section = section.buffer().map_err(ReadError::Io)?;
            self.read_block_value_changes(section, &time, &mask, &mut callback)?;
        }
        Ok(())
    }

    fn read_block_value_changes<E>(
        &self,
        section: Section<Cursor<Vec<u8>>>,
        time: &TimeFilter,
        mask: &[bool],
        callback: &mut impl FnMut(u64, FstSignalHandle, Value) -> Result<(), E>,
    ) -> Result<(), ReadError<E>> {
        let mut block = section.read_value_changes().map_err(ReadError::Io)?;
        let head = block.read_head_layout().map_err(ReadError::Io)?;
        let tail = block.read_tail_layout().map_err(ReadError::Io)?;
        let positions = block.read_positions(&head, &tail).map_err(ReadError::Io)?;
        let times = block.read_time_table(&tail).map_err(ReadError::Io)?;

        // The time table is strictly increasing, so the inclusive window maps
        // to a contiguous time-index range, found once by binary search.
        let lo = times.partition_point(|&t| t < time.start) as u64;
        let hi = times.partition_point(|&t| t <= time.end) as u64;
        let table_len = times.len() as u64;

        for (geometry_index, &masked) in mask.iter().enumerate() {
            if !masked {
                continue;
            }
            let id = geometry_index as u64;
            let length = self.geometry[geometry_index];
            let waves = block
                .read_waves(&head, &positions, id, length)
                .map_err(ReadError::Io)?;
            let handle = FstSignalHandle::from_index(geometry_index);
            for change in waves.iter() {
                let (time_index, value) = change.map_err(ReadError::Io)?;
                if time_index >= table_len {
                    return Err(ReadError::Io(invalid_data("time index out of range")));
                }
                if time_index < lo || time_index >= hi {
                    continue;
                }
                callback(times[time_index as usize], handle, value).map_err(ReadError::Callback)?;
            }
        }
        Ok(())
    }
}

/// An error from [`FstReader::read_value_changes`]: an I/O or parse error, or
/// an error the caller's callback returned.
#[derive(Debug)]
pub enum ReadError<E> {
    Io(std::io::Error),
    Callback(E),
}

impl<E: std::fmt::Display> std::fmt::Display for ReadError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(err) => write!(f, "{err}"),
            ReadError::Callback(err) => write!(f, "{err}"),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for ReadError<E> {}

/// Dispatches one hierarchy entry to `callback` with the current scope stack,
/// then updates the stack: a `Scope` is reported before it is pushed, an
/// `UpScope` before the enclosing scope is popped.
fn visit_hierarchy_entry(
    entry: &HierarchyEntry,
    scopes: &mut Vec<Vec<u8>>,
    callback: &mut impl FnMut(&HierarchyEntry, &[Vec<u8>]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    callback(entry, scopes)?;
    match entry {
        HierarchyEntry::Scope { name, .. } => scopes.push(name.clone()),
        HierarchyEntry::UpScope => {
            scopes.pop();
        }
        _ => {}
    }
    Ok(())
}

/// Defines a `#[repr(u8)]` enum whose discriminants run from 0 in
/// declaration order, with `ALL` and `from_u8`.
macro_rules! format_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$variant_meta:meta])* $variant:ident,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
        }

        impl $name {
            /// Every variant, in discriminant order.
            pub const ALL: &[$name] = &[$($name::$variant,)*];

            pub fn from_u8(value: u8) -> Option<$name> {
                Self::ALL.get(usize::from(value)).copied()
            }
        }
    };
}

format_enum! {
    /// A scope type (FST_ST_*).
    ScopeType {
        Module,
        Task,
        Function,
        Begin,
        Fork,
        Generate,
        Struct,
        Union,
        Class,
        Interface,
        Package,
        Program,
        VhdlArchitecture,
        VhdlProcedure,
        VhdlFunction,
        VhdlRecord,
        VhdlProcess,
        VhdlBlock,
        VhdlForGenerate,
        VhdlIfGenerate,
        VhdlGenerate,
        VhdlPackage,
        SvArray,
    }
}

format_enum! {
    /// A variable type (FST_VT_*).
    VarType {
        Event,
        Integer,
        Parameter,
        Real,
        RealParameter,
        Reg,
        Supply0,
        Supply1,
        Time,
        Tri,
        TriAnd,
        TriOr,
        TriReg,
        Tri0,
        Tri1,
        WAnd,
        Wire,
        WOr,
        Port,
        SparseArray,
        RealTime,
        /// A variable-length string.
        String,
        Bit,
        Logic,
        Int,
        ShortInt,
        LongInt,
        Byte,
        Enum,
        /// Written as a 64-bit real, like the other real types.
        ShortReal,
    }
}

format_enum! {
    /// A variable direction (FST_VD_*).
    VarDirection {
        Implicit,
        Input,
        Output,
        InOut,
        Buffer,
        Linkage,
    }
}

format_enum! {
    /// An attribute type (FST_AT_*).
    AttributeType {
        Misc,
        Array,
        Enum,
        Pack,
    }
}

format_enum! {
    /// The source language of a design (FST_FT_*).
    FileType {
        Verilog,
        Vhdl,
        VerilogVhdl,
    }
}

/// Name of a variable type (FST_VT_*), or `None` if unknown.
pub fn var_type_name(var_type: u8) -> Option<&'static str> {
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
pub fn var_direction_name(direction: u8) -> Option<&'static str> {
    const NAMES: [&str; 6] = ["", "input", "output", "inout", "buffer", "linkage"];
    NAMES.get(usize::from(direction)).copied()
}

/// Name of a scope type (FST_ST_*), or `None` if unknown.
pub fn scope_type_name(scope_type: u8) -> Option<&'static str> {
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

/// Encodes an FST variable id as a VCD identifier code: base 94 starting at
/// '!', least significant digit first. The id is 1-based (0 is illegal).
///
/// This matches `fstVcdID` in the reference C library.
pub fn vcd_id(mut id: u64) -> String {
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
/// [`MINIMAL_ALPHABET`], used when renumbering a filtered set of signals. This
/// is bijective base-94 written most-significant digit first, so codes grow in
/// lexical order over the alphabet: `a`, `b`, ... then `aa`, `ab`, ... (not
/// `aa`, `ba`). The ordinal is 1-based (0 is illegal).
pub fn minimal_vcd_id(mut n: u64) -> String {
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

impl HierarchyEntry {
    /// Appends the entry as a hierarchy record, the inverse of
    /// [`HierarchyEntries::read_entry`].
    ///
    /// A non-aliased variable's `id` is not stored: readers number variables
    /// in record order.
    pub fn write_record(&self, out: &mut Vec<u8>) {
        match self {
            HierarchyEntry::AttributeBegin {
                attr_type,
                subtype,
                name,
                value,
            } => {
                out.extend_from_slice(&[TAG_ATTRIBUTE_BEGIN, *attr_type, *subtype]);
                write_name(out, name);
                write_varint(out, *value);
            }
            HierarchyEntry::AttributeEnd => out.push(TAG_ATTRIBUTE_END),
            HierarchyEntry::Scope {
                scope_type,
                name,
                component,
            } => {
                out.extend_from_slice(&[TAG_SCOPE, *scope_type]);
                write_name(out, name);
                write_name(out, component);
            }
            HierarchyEntry::UpScope => out.push(TAG_UPSCOPE),
            HierarchyEntry::Var {
                var_type,
                direction,
                name,
                length,
                id,
                is_alias,
            } => {
                out.extend_from_slice(&[*var_type, *direction]);
                write_name(out, name);
                write_varint(out, *length);
                write_varint(out, if *is_alias { id + 1 } else { 0 });
            }
        }
    }
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
    /// Parses a raw, uncompressed hierarchy record stream.
    ///
    /// A simulator writes the design hierarchy to a sibling `<file>.fst.hier`
    /// while the dump is still in progress; that file holds the same records as
    /// a hierarchy section body, but without the section header, length prefix,
    /// or compression wrapper that [`HierarchyBlock::entries`] strips.
    pub fn from_records(reader: R) -> HierarchyEntries<R> {
        HierarchyEntries {
            data: reader,
            next_id: 0,
            done: false,
        }
    }

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

/// Appends a null terminated name, the inverse of [`read_name`].
fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.extend_from_slice(name);
    out.push(0);
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

/// Maps each byte to its 8 bits as ASCII `'0'`/`'1'`, most-significant bit
/// first, so a packed multi-bit value expands a whole byte at a time.
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

impl Value<'_> {
    /// Returns the value with one ASCII character per bit. For reals and
    /// variable length values, returns the raw bytes.
    pub fn to_ascii(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_ascii(&mut out);
        out
    }

    /// Appends the value's ASCII form (one character per bit) to `out`. For
    /// reals and variable length values, appends the raw bytes. Lets a caller
    /// reuse a buffer instead of allocating a `Vec` per value.
    pub fn write_ascii(&self, out: &mut Vec<u8>) {
        match *self {
            Value::Bit(bit) => out.push(bit),
            Value::Packed { data, bits } => {
                let full = bits / 8;
                for &byte in &data[..full] {
                    out.extend_from_slice(&BIT_EXPAND[byte as usize]);
                }
                let rem = bits % 8;
                if rem != 0 {
                    out.extend_from_slice(&BIT_EXPAND[data[full] as usize][..rem]);
                }
            }
            Value::Ascii(data) | Value::VarLen(data) => out.extend_from_slice(data),
            Value::Real(bytes) => out.extend_from_slice(&bytes),
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

    /// The clock level of a single-bit value: the ASCII bit (`b'0'`/`b'1'`) of
    /// a defined single-bit value, or `None` for `x`, `z`, a vector, or a
    /// real.
    pub fn clock_level(&self) -> Option<u8> {
        match *self {
            Value::Bit(b @ (b'0' | b'1')) => Some(b),
            Value::Ascii([b @ (b'0' | b'1')]) => Some(*b),
            Value::Packed { bits: 1, data } => match b'0' | (data[0] >> 7) {
                b @ (b'0' | b'1') => Some(b),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The signals of a design: their geometry and where each one's value sits in
/// a block's frame.
#[derive(Debug)]
struct Layout {
    geometry: Vec<VariableLength>,
    frame_offsets: Vec<usize>,
    frame_len: usize,
}

impl Layout {
    fn new(geometry: &[VariableLength]) -> std::io::Result<Layout> {
        if geometry.len() > HANDLE_MASK as usize {
            return Err(invalid_input("too many signals"));
        }
        let mut frame_offsets = Vec::with_capacity(geometry.len());
        let mut frame_len = 0usize;
        for &length in geometry {
            frame_offsets.push(frame_len);
            frame_len += match length {
                VariableLength::Bits(bits) if (1..0xFFFF_FFFF).contains(&bits) => bits as usize,
                VariableLength::Bits(_) => return Err(invalid_input("invalid signal length")),
                VariableLength::Real => 8,
                VariableLength::VarLen => 0,
            };
        }
        Ok(Layout {
            geometry: geometry.to_vec(),
            frame_offsets,
            frame_len,
        })
    }

    /// The frame before any change: `x` for every bit and NaN for reals.
    fn initial_frame(&self, frame: &mut Vec<u8>) {
        frame.clear();
        frame.resize(self.frame_len, b'x');
        for (&length, &offset) in self.geometry.iter().zip(&self.frame_offsets) {
            if length == VariableLength::Real {
                frame[offset..offset + 8].copy_from_slice(&f64::NAN.to_le_bytes());
            }
        }
    }
}

/// The form a buffered change was given in. Converting it to the form the
/// file stores is left to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeForm {
    /// A 1-bit value; `data` is its ASCII byte.
    Bit,
    /// `ceil(n / 8)` bytes, most significant bit first.
    Packed,
    /// `n` ASCII bytes.
    Ascii,
    /// `ceil(n / 8)` bytes, least significant byte first.
    Le,
    /// Two `ceil(n / 8)` byte planes, least significant byte first: the
    /// value bits, then the bits that are x or z.
    FourState,
    /// 8 bytes, little-endian.
    Real,
    /// A little-endian `u32` length, then the bytes.
    VarLen,
    /// A two-state value of at most 32 bits; `data` is the value itself.
    Word,
}

#[derive(Debug, Clone, Copy)]
struct BufferedChange {
    /// The signal's index in the low [`HANDLE_BITS`] bits, and the
    /// [`ChangeForm`] above them.
    handle_form: u32,
    time_index: u32,
    /// Offset of the value in the payload, or the value itself for
    /// [`ChangeForm::Bit`].
    data: u32,
}

const HANDLE_BITS: u32 = 29;
const HANDLE_MASK: u32 = (1 << HANDLE_BITS) - 1;

impl BufferedChange {
    fn new(handle: u32, time_index: u32, form: ChangeForm, data: u32) -> BufferedChange {
        BufferedChange {
            handle_form: handle | (form as u32) << HANDLE_BITS,
            time_index,
            data,
        }
    }

    fn handle(self) -> usize {
        (self.handle_form & HANDLE_MASK) as usize
    }

    fn form(self) -> ChangeForm {
        match self.handle_form >> HANDLE_BITS {
            0 => ChangeForm::Bit,
            1 => ChangeForm::Packed,
            2 => ChangeForm::Ascii,
            3 => ChangeForm::Le,
            4 => ChangeForm::FourState,
            5 => ChangeForm::Real,
            6 => ChangeForm::VarLen,
            _ => ChangeForm::Word,
        }
    }
}

const NO_CHANGE: u32 = u32::MAX;

/// What [`BlockBuffer`] needs of a signal on every change, kept together so
/// that recording a change touches one entry.
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// Index in `changes` of the signal's last change, or [`NO_CHANGE`].
    last_change: u32,
    /// The length in bits, or [`SLOT_REAL`] or [`SLOT_VAR_LEN`].
    width: u32,
    /// Number of changes of the signal in the block.
    changes: u32,
}

const SLOT_REAL: u32 = u32::MAX;
const SLOT_VAR_LEN: u32 = 0;

impl Slot {
    fn new(length: VariableLength) -> Slot {
        let width = match length {
            VariableLength::Bits(bits) => bits as u32,
            VariableLength::Real => SLOT_REAL,
            VariableLength::VarLen => SLOT_VAR_LEN,
        };
        Slot {
            last_change: NO_CHANGE,
            width,
            changes: 0,
        }
    }

    fn length(self) -> VariableLength {
        match self.width {
            SLOT_REAL => VariableLength::Real,
            SLOT_VAR_LEN => VariableLength::VarLen,
            bits => VariableLength::Bits(u64::from(bits)),
        }
    }
}

const PLACEHOLDER_CHANGE: BufferedChange = BufferedChange {
    handle_form: 0,
    time_index: 0,
    data: 0,
};

/// Everything one value-change section holds before encoding: the frame (the
/// value of every signal at the block's first time), the time table, and the
/// changes, kept in the form they were given.
///
/// The first block of a file is made by [`new`](Self::new); each later one is
/// started from the block before it by [`start_next`](Self::start_next), so
/// that it begins at the time the previous one ends and its frame holds the
/// values the previous one left. Changes are recorded at the last time pushed
/// (or, in the first block, before any time is pushed, at the first time).
/// They are checked against the signal's length as they are recorded, so a
/// block that was filled without error always encodes.
#[derive(Debug)]
pub struct BlockBuffer {
    layout: Arc<Layout>,
    first: bool,
    frame: Vec<u8>,
    times: Vec<u64>,
    changes: Vec<BufferedChange>,
    payload: Vec<u8>,
    slots: Vec<Slot>,
    /// The signals with a change in this block, in first-change order.
    changed: Vec<u32>,
}

impl BlockBuffer {
    /// The first block of a file whose signals have the given geometry,
    /// indexed by [`FstSignalHandle::index`].
    pub fn new(geometry: &[VariableLength]) -> std::io::Result<BlockBuffer> {
        let layout = Arc::new(Layout::new(geometry)?);
        let mut frame = Vec::new();
        layout.initial_frame(&mut frame);
        Ok(BlockBuffer {
            slots: layout
                .geometry
                .iter()
                .map(|&length| Slot::new(length))
                .collect(),
            layout,
            first: true,
            frame,
            times: Vec::new(),
            changes: Vec::new(),
            payload: Vec::new(),
            changed: Vec::new(),
        })
    }

    /// A block without signals, for [`start_next`](Self::start_next) to
    /// reset.
    fn placeholder() -> BlockBuffer {
        BlockBuffer::new(&[]).expect("an empty geometry is valid")
    }

    /// The geometry the block was made for.
    pub fn geometry(&self) -> &[VariableLength] {
        &self.layout.geometry
    }

    /// The time table.
    pub fn times(&self) -> &[u64] {
        &self.times
    }

    pub fn start_time(&self) -> Option<u64> {
        self.times.first().copied()
    }

    pub fn end_time(&self) -> Option<u64> {
        self.times.last().copied()
    }

    /// Bytes buffered for the changes and the time table.
    pub fn len_bytes(&self) -> usize {
        self.changes.len() * std::mem::size_of::<BufferedChange>()
            + self.payload.len()
            + self.times.len() * 8
    }

    /// Whether the block holds a time step of its own: any time in the first
    /// block, or one past the time a later block starts at.
    pub fn has_time_step(&self) -> bool {
        self.times.len() > usize::from(!self.first)
    }

    /// Starts a new time step. Pushing the last time again does nothing.
    pub fn push_time(&mut self, time: u64) -> std::io::Result<()> {
        match self.times.last() {
            Some(&last) if time < last => Err(invalid_input("time goes backwards")),
            Some(&last) if time == last => Ok(()),
            _ => {
                if u32::try_from(self.times.len()).is_err() {
                    return Err(invalid_input("too many time steps in one block"));
                }
                self.times.push(time);
                Ok(())
            }
        }
    }

    /// Resets `next` to the block that follows this one, reusing its
    /// allocations: it starts at this block's end time, with this block's
    /// final values as its frame.
    pub fn start_next(&self, next: &mut BlockBuffer) -> std::io::Result<()> {
        let end = self
            .end_time()
            .ok_or_else(|| invalid_input("block has no time step"))?;

        let n = self.layout.geometry.len();
        if Arc::ptr_eq(&next.layout, &self.layout) && next.slots.len() == n {
            for &handle in &next.changed {
                let slot = &mut next.slots[handle as usize];
                slot.last_change = NO_CHANGE;
                slot.changes = 0;
            }
        } else {
            next.layout = Arc::clone(&self.layout);
            next.slots.clear();
            next.slots
                .extend(self.layout.geometry.iter().map(|&length| Slot::new(length)));
        }
        next.changed.clear();
        next.changes.clear();
        next.payload.clear();
        next.times.clear();
        next.times.push(end);
        next.first = false;

        next.frame.clear();
        next.frame.extend_from_slice(&self.frame);
        let mut value = Vec::new();
        for &handle in &self.changed {
            let handle = handle as usize;
            let length = self.layout.geometry[handle];
            let change = self.changes[self.slots[handle].last_change as usize];
            let offset = self.layout.frame_offsets[handle];
            let packed = wave_value(change, &self.payload, length, &mut value);
            match length {
                VariableLength::VarLen => {}
                VariableLength::Bits(bits) if packed => {
                    let frame = &mut next.frame[offset..offset + bits as usize];
                    for (i, ascii) in frame.iter_mut().enumerate() {
                        *ascii = b'0' | ((value[i / 8] >> (7 - i % 8)) & 1);
                    }
                }
                _ => next.frame[offset..offset + value.len()].copy_from_slice(&value),
            }
        }
        Ok(())
    }

    fn current_index(&self) -> usize {
        self.times.len().saturating_sub(1)
    }

    /// Checks that `handle` may change at `time_index` and returns its length.
    fn check(&self, handle: FstSignalHandle, time_index: usize) -> std::io::Result<VariableLength> {
        let slot = *self
            .slots
            .get(handle.index())
            .ok_or_else(|| invalid_input("signal handle out of range"))?;
        let first_index = usize::from(!self.first);
        if time_index < first_index || time_index > self.current_index() {
            return Err(invalid_input("time index out of range"));
        }
        let last = slot.last_change;
        // A change at the current time index is never out of order, so the
        // previous change is only read back from the log for an earlier one.
        if time_index != self.current_index()
            && last != NO_CHANGE
            && self.changes[last as usize].time_index as usize > time_index
        {
            return Err(invalid_input("changes of a signal must be in time order"));
        }
        if u32::try_from(self.changes.len() + 1).is_err() {
            return Err(invalid_input("too many changes in one block"));
        }
        Ok(slot.length())
    }

    /// Like [`check`](Self::check), for a change at the current time index,
    /// which needs no time-order check.
    fn check_current(&self, handle: FstSignalHandle) -> std::io::Result<VariableLength> {
        let slot = *self
            .slots
            .get(handle.index())
            .ok_or_else(|| invalid_input("signal handle out of range"))?;
        if !self.has_time_step() && !self.first {
            return Err(invalid_input("time index out of range"));
        }
        if self.changes.len() >= NO_CHANGE as usize {
            return Err(invalid_input("too many changes in one block"));
        }
        Ok(slot.length())
    }

    fn push(&mut self, handle: FstSignalHandle, time_index: usize, form: ChangeForm, data: u32) {
        let index = self.changes.len() as u32;
        self.changes.push(BufferedChange::new(
            handle.index() as u32,
            time_index as u32,
            form,
            data,
        ));
        let slot = &mut self.slots[handle.index()];
        if slot.last_change == NO_CHANGE {
            self.changed.push(handle.index() as u32);
        }
        slot.last_change = index;
        slot.changes += 1;
    }

    fn push_payload(
        &mut self,
        handle: FstSignalHandle,
        time_index: usize,
        form: ChangeForm,
        bytes: &[&[u8]],
    ) -> std::io::Result<()> {
        let offset =
            u32::try_from(self.payload.len()).map_err(|_| invalid_input("block buffer is full"))?;
        for part in bytes {
            self.payload.extend_from_slice(part);
        }
        self.push(handle, time_index, form, offset);
        Ok(())
    }

    fn push_bit(
        &mut self,
        handle: FstSignalHandle,
        time_index: usize,
        ascii: u8,
    ) -> std::io::Result<()> {
        let ascii = match ascii {
            b'0' | b'1' | b'x' | b'z' | b'h' | b'u' | b'w' | b'l' | b'-' | b'?' => ascii,
            b'X' | b'Z' | b'H' | b'U' | b'W' | b'L' => ascii.to_ascii_lowercase(),
            _ => return Err(invalid_input("not a 1-bit value")),
        };
        self.push(handle, time_index, ChangeForm::Bit, u32::from(ascii));
        Ok(())
    }

    /// Records a value of any form the reader yields. [`Value::Real`] bytes
    /// are written as they are, so they must be little-endian.
    pub fn change(&mut self, handle: FstSignalHandle, value: Value<'_>) -> std::io::Result<()> {
        let length = self.check_current(handle)?;
        self.push_value(self.current_index(), handle, length, value)
    }

    /// Like [`change`](Self::change), at an earlier time step of this block:
    /// `time_index` indexes [`times`](Self::times). A signal's changes must
    /// be recorded in time order; a later change at the same time index
    /// replaces the earlier one.
    pub fn change_at(
        &mut self,
        time_index: usize,
        handle: FstSignalHandle,
        value: Value<'_>,
    ) -> std::io::Result<()> {
        let length = self.check(handle, time_index)?;
        self.push_value(time_index, handle, length, value)
    }

    fn push_value(
        &mut self,
        time_index: usize,
        handle: FstSignalHandle,
        length: VariableLength,
        value: Value<'_>,
    ) -> std::io::Result<()> {
        match (length, value) {
            (VariableLength::Bits(1), Value::Bit(ascii) | Value::Ascii(&[ascii])) => {
                self.push_bit(handle, time_index, ascii)
            }
            (
                VariableLength::Bits(1),
                Value::Packed {
                    data: &[byte],
                    bits: 1,
                },
            ) => self.push_bit(handle, time_index, b'0' | (byte >> 7)),
            (VariableLength::Bits(n), Value::Packed { data, bits })
                if bits as u64 == n && data.len() == bits.div_ceil(8) =>
            {
                self.push_payload(handle, time_index, ChangeForm::Packed, &[data])
            }
            (VariableLength::Bits(n), Value::Ascii(ascii)) if ascii.len() as u64 == n => {
                self.push_payload(handle, time_index, ChangeForm::Ascii, &[ascii])
            }
            (VariableLength::Real, Value::Real(bytes)) => {
                self.push_payload(handle, time_index, ChangeForm::Real, &[&bytes])
            }
            (VariableLength::VarLen, Value::VarLen(bytes)) => {
                let len = u32::try_from(bytes.len())
                    .map_err(|_| invalid_input("variable-length value too long"))?;
                self.push_payload(
                    handle,
                    time_index,
                    ChangeForm::VarLen,
                    &[&len.to_le_bytes(), bytes],
                )
            }
            _ => Err(invalid_input("value does not match the signal's length")),
        }
    }

    /// Records a value of a 1-bit signal.
    pub fn change_bit(&mut self, handle: FstSignalHandle, bit: bool) -> std::io::Result<()> {
        match self.check_current(handle)? {
            VariableLength::Bits(1) => {
                let ascii = b'0' | u8::from(bit);
                self.push(
                    handle,
                    self.current_index(),
                    ChangeForm::Bit,
                    u32::from(ascii),
                );
                Ok(())
            }
            _ => Err(invalid_input("value does not match the signal's length")),
        }
    }

    /// Records the low `n` bits of `value`, for an n-bit signal with
    /// `n <= 64`.
    pub fn change_u64(&mut self, handle: FstSignalHandle, value: u64) -> std::io::Result<()> {
        self.change_le(handle, &[value], None)
    }

    /// Records an n-bit value given as `ceil(n / 32)` words, least
    /// significant word first, like Verilator's `VlWide` and libfst's
    /// `fstWriterEmitValueChangeVec32`. Bits past `n` are ignored.
    pub fn change_words32(
        &mut self,
        handle: FstSignalHandle,
        words: &[u32],
    ) -> std::io::Result<()> {
        self.change_le(handle, words, None)
    }

    /// Like [`change_words32`](Self::change_words32), with 64-bit words.
    pub fn change_words64(
        &mut self,
        handle: FstSignalHandle,
        words: &[u64],
    ) -> std::io::Result<()> {
        self.change_le(handle, words, None)
    }

    /// Records a four-state value as VPI-style planes of 32-bit words, least
    /// significant word first: each bit is 0 = (0, 0), 1 = (1, 0), z = (0, 1)
    /// and x = (1, 1) in (`aval`, `bval`).
    pub fn change_four_state(
        &mut self,
        handle: FstSignalHandle,
        aval: &[u32],
        bval: &[u32],
    ) -> std::io::Result<()> {
        self.change_le(handle, aval, Some(bval))
    }

    /// Records a value given as little-endian words, with the x/z plane of a
    /// four-state value.
    fn change_le<T: LeWord>(
        &mut self,
        handle: FstSignalHandle,
        aval: &[T],
        bval: Option<&[T]>,
    ) -> std::io::Result<()> {
        let time_index = self.current_index();
        let VariableLength::Bits(n) = self.check_current(handle)? else {
            return Err(invalid_input("value does not match the signal's length"));
        };
        let n = n as usize;
        if aval.len() != n.div_ceil(T::BYTES * 8)
            || bval.is_some_and(|bval| bval.len() != aval.len())
        {
            return Err(invalid_input("value does not match the signal's length"));
        }
        if n == 1 {
            let bval = bval.map_or(0, |bval| bval[0].to_le()[0] & 1);
            let ascii = four_state_ascii(aval[0].to_le()[0] & 1, bval);
            return self.push_bit(handle, time_index, ascii);
        }
        if n <= 32 && bval.is_none() {
            let low = aval[0].to_le();
            let word = u32::from_le_bytes([low[0], low[1], low[2], low[3]]);
            self.push(handle, time_index, ChangeForm::Word, word);
            return Ok(());
        }
        let offset =
            u32::try_from(self.payload.len()).map_err(|_| invalid_input("block buffer is full"))?;
        let len = n.div_ceil(8);
        extend_le(&mut self.payload, aval, len);
        let form = match bval {
            None => ChangeForm::Le,
            Some(bval) => {
                extend_le(&mut self.payload, bval, len);
                ChangeForm::FourState
            }
        };
        self.push(handle, time_index, form, offset);
        Ok(())
    }

    /// Records a real.
    pub fn change_f64(&mut self, handle: FstSignalHandle, value: f64) -> std::io::Result<()> {
        self.change(handle, Value::Real(value.to_le_bytes()))
    }

    /// Records a value with one ASCII byte per bit, of the signal's exact
    /// width. A 1-bit value must be one of `01xzhuwl-?` (or upper case).
    pub fn change_ascii(&mut self, handle: FstSignalHandle, ascii: &[u8]) -> std::io::Result<()> {
        self.change(handle, Value::Ascii(ascii))
    }

    /// Records a value of a variable-length signal, such as a string.
    pub fn change_var_len(&mut self, handle: FstSignalHandle, bytes: &[u8]) -> std::io::Result<()> {
        self.change(handle, Value::VarLen(bytes))
    }
}

/// A word of a value given least significant word first.
trait LeWord: Copy {
    const BYTES: usize;
    /// The word's little-endian bytes, zero-padded to 8.
    fn to_le(self) -> [u8; 8];
}

impl LeWord for u32 {
    const BYTES: usize = 4;
    fn to_le(self) -> [u8; 8] {
        u64::from(self).to_le_bytes()
    }
}

impl LeWord for u64 {
    const BYTES: usize = 8;
    fn to_le(self) -> [u8; 8] {
        self.to_le_bytes()
    }
}

/// Appends the first `len` bytes of the little-endian value made of `words`.
fn extend_le<T: LeWord>(out: &mut Vec<u8>, words: &[T], len: usize) {
    for (i, word) in words.iter().enumerate() {
        let start = i * T::BYTES;
        if start >= len {
            break;
        }
        out.extend_from_slice(&word.to_le()[..(len - start).min(T::BYTES)]);
    }
}

/// The ASCII byte of a four-state bit given as its (aval, bval) pair.
fn four_state_ascii(aval: u8, bval: u8) -> u8 {
    match (aval, bval) {
        (a, 0) => b'0' | a,
        (0, _) => b'z',
        _ => b'x',
    }
}

/// Writes a buffered change into `out` in the form the waves store it: the
/// ASCII byte of a 1-bit value, the packed bits (returning true) or ASCII
/// bytes of a wider one, the 8 bytes of a real, or a variable-length value's
/// bytes.
///
/// A value with only 0s and 1s is always packed, so two equal values give
/// equal bytes whatever form they were given in.
fn wave_value(
    change: BufferedChange,
    payload: &[u8],
    length: VariableLength,
    out: &mut Vec<u8>,
) -> bool {
    out.clear();
    let start = change.data as usize;
    let bits = match length {
        VariableLength::Bits(bits) => bits as usize,
        _ => 0,
    };
    let len = bits.div_ceil(8);
    match change.form() {
        ChangeForm::Bit => {
            out.push(change.data as u8);
            false
        }
        ChangeForm::Real => {
            out.extend_from_slice(&payload[start..start + 8]);
            false
        }
        ChangeForm::VarLen => {
            let len = u32::from_le_bytes(payload[start..start + 4].try_into().unwrap()) as usize;
            out.extend_from_slice(&payload[start + 4..start + 4 + len]);
            false
        }
        ChangeForm::Packed => {
            out.extend_from_slice(&payload[start..start + len]);
            if bits % 8 != 0 {
                out[len - 1] &= 0xFF << (8 - bits % 8);
            }
            true
        }
        ChangeForm::Ascii => pack_ascii(&payload[start..start + bits], out),
        ChangeForm::Le => {
            pack_le(&payload[start..start + len], bits, out);
            true
        }
        ChangeForm::Word => {
            let left_aligned = u64::from(change.data) << (64 - bits);
            out.extend_from_slice(&left_aligned.to_be_bytes()[..len]);
            true
        }
        ChangeForm::FourState => {
            let aval = &payload[start..start + len];
            let bval = &payload[start + len..start + 2 * len];
            let bit = |plane: &[u8], i: usize| (plane[i / 8] >> (i % 8)) & 1;
            if (0..bits).all(|i| bit(bval, i) == 0) {
                pack_le(aval, bits, out);
                true
            } else {
                out.extend(
                    (0..bits)
                        .rev()
                        .map(|i| four_state_ascii(bit(aval, i), bit(bval, i))),
                );
                false
            }
        }
    }
}

/// Packs an ASCII value into `out` when it has only 0s and 1s, returning
/// true, and copies it otherwise.
fn pack_ascii(ascii: &[u8], out: &mut Vec<u8>) -> bool {
    if !ascii.iter().all(|&b| b == b'0' || b == b'1') {
        out.extend_from_slice(ascii);
        return false;
    }
    let start = out.len();
    out.resize(start + ascii.len().div_ceil(8), 0);
    for (i, &b) in ascii.iter().enumerate() {
        out[start + i / 8] |= (b & 1) << (7 - i % 8);
    }
    true
}

/// Packs the low `bits` bits of a little-endian value, most significant bit
/// first, into `ceil(bits / 8)` bytes, left-aligned in the last byte.
fn pack_le(le: &[u8], bits: usize, out: &mut Vec<u8>) {
    let len = bits.div_ceil(8);
    let pad = len * 8 - bits;
    for k in (0..len).rev() {
        let low = if pad != 0 && k > 0 {
            le[k - 1] >> (8 - pad)
        } else {
            0
        };
        out.push((le[k] << pad) | low);
    }
}

/// How the waves of a value-change block are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaveCompression {
    /// LZ4, for each wave that it shrinks.
    #[default]
    Lz4,
    /// Every wave is stored raw.
    None,
}

/// Options of [`BlockEncoder`].
#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeOptions {
    /// Drop a change equal to the signal's previous value. Changes at the
    /// file's first time step are always kept, so it lists every signal set
    /// there. Variable-length values are never dropped.
    pub deduplicate: bool,
    pub wave_compression: WaveCompression,
}

/// Encodes [`BlockBuffer`]s as value-change sections, reusing its scratch
/// buffers from one block to the next.
///
/// The bytes depend only on the block and the options, so blocks may be
/// encoded on any thread and in any order.
pub struct BlockEncoder {
    options: EncodeOptions,
    /// `order[starts[i]..starts[i + 1]]` are the changes of signal `i`.
    starts: Vec<u32>,
    cursor: Vec<u32>,
    /// The block's changes, grouped by signal and in time order within each.
    order: Vec<BufferedChange>,
    value: Vec<u8>,
    previous: Vec<u8>,
    /// The uncompressed waves stored so far, to find dynamic aliases.
    waves: Vec<u8>,
    /// Hash of a wave to the last entry of `stored` with that hash.
    stored_by_hash: std::collections::HashMap<u64, u32>,
    stored: Vec<StoredWave>,
    positions: Vec<WavePosition>,
    pack: Vec<u8>,
    lz4: Vec<u8>,
    /// LZ4 hash tables for inputs shorter than 64 KiB and for longer ones,
    /// the split `lz4_flex::block::compress_into` makes.
    lz4_small: lz4_flex::block::CompressTable,
    lz4_large: lz4_flex::block::CompressTable,
    zlib: Vec<u8>,
    table: Vec<u8>,
}

/// A wave stored in the block, for dynamic alias lookup.
struct StoredWave {
    signal: u32,
    /// Range in [`BlockEncoder::waves`].
    start: usize,
    end: usize,
    /// The previous entry with the same hash, or [`NO_CHANGE`].
    next: u32,
}

impl BlockEncoder {
    pub fn new(options: EncodeOptions) -> BlockEncoder {
        BlockEncoder {
            options,
            starts: Vec::new(),
            cursor: Vec::new(),
            order: Vec::new(),
            value: Vec::new(),
            previous: Vec::new(),
            waves: Vec::new(),
            stored_by_hash: std::collections::HashMap::new(),
            stored: Vec::new(),
            positions: Vec::new(),
            pack: Vec::new(),
            lz4: Vec::new(),
            lz4_small: lz4_flex::block::CompressTable::small(),
            lz4_large: lz4_flex::block::CompressTable::large(),
            zlib: Vec::new(),
            table: Vec::new(),
        }
    }

    /// Encodes `block` as a complete FST_BL_VCDATA_DYN_ALIAS2 section (block
    /// type, length and data) into `out`, replacing its contents.
    pub fn encode(&mut self, block: &BlockBuffer, out: &mut Vec<u8>) -> std::io::Result<()> {
        let (Some(start_time), Some(end_time)) = (block.start_time(), block.end_time()) else {
            return Err(invalid_input("block has no time step"));
        };
        let count = block.layout.geometry.len() as u64;

        self.sort(block);
        let memory_required = self.encode_waves(block);

        out.clear();
        out.push(BlockType::ValueChangeDataDynamicAlias2 as u8);
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&start_time.to_be_bytes());
        out.extend_from_slice(&end_time.to_be_bytes());
        out.extend_from_slice(&memory_required.to_be_bytes());

        let frame = zlib_if_smaller(&block.frame, 4, &mut self.zlib).unwrap_or(&block.frame);
        write_varint(out, block.frame.len() as u64);
        write_varint(out, frame.len() as u64);
        write_varint(out, count);
        out.extend_from_slice(frame);

        write_varint(out, count);
        out.push(b'4');
        out.extend_from_slice(&self.pack);

        let positions_start = out.len();
        write_positions(&self.positions, out);
        let positions_length = (out.len() - positions_start) as u64;
        out.extend_from_slice(&positions_length.to_be_bytes());

        self.table.clear();
        let mut previous = 0;
        for &time in &block.times {
            write_varint(&mut self.table, time - previous);
            previous = time;
        }
        let times = zlib_if_smaller(&self.table, 9, &mut self.zlib).unwrap_or(&self.table);
        out.extend_from_slice(times);
        out.extend_from_slice(&(self.table.len() as u64).to_be_bytes());
        out.extend_from_slice(&(times.len() as u64).to_be_bytes());
        out.extend_from_slice(&(block.times.len() as u64).to_be_bytes());

        // The section length includes its own 8-byte length field.
        let len = (out.len() - 1) as u64;
        out[1..9].copy_from_slice(&len.to_be_bytes());
        Ok(())
    }

    /// Counting sort of the block's changes by signal, which keeps them in
    /// time order within each signal.
    fn sort(&mut self, block: &BlockBuffer) {
        let count = block.layout.geometry.len();
        self.starts.clear();
        self.starts.resize(count + 1, 0);
        for (i, slot) in block.slots.iter().enumerate() {
            self.starts[i + 1] = self.starts[i] + slot.changes;
        }
        self.cursor.clear();
        self.cursor.extend_from_slice(&self.starts[..count]);
        // Every entry up to the block's change count is overwritten below, so
        // the vector only grows and keeps whatever a larger block left past it.
        if self.order.len() < block.changes.len() {
            self.order.resize(block.changes.len(), PLACEHOLDER_CHANGE);
        }
        for change in &block.changes {
            let slot = &mut self.cursor[change.handle()];
            self.order[*slot as usize] = *change;
            *slot += 1;
        }
    }

    /// Builds and stores the waves of every signal into `pack`, fills
    /// `positions`, and returns the total uncompressed wave length.
    fn encode_waves(&mut self, block: &BlockBuffer) -> u64 {
        self.waves.clear();
        self.stored_by_hash.clear();
        self.stored.clear();
        self.positions.clear();
        self.pack.clear();
        let mut memory_required = 0u64;

        for signal in 0..block.layout.geometry.len() {
            let start = self.waves.len();
            let changes = self.starts[signal] as usize..self.starts[signal + 1] as usize;
            if !changes.is_empty() {
                self.write_wave(block, signal, changes);
            }
            let end = self.waves.len();
            if start == end {
                self.positions.push(WavePosition::None);
                continue;
            }
            memory_required += (end - start) as u64;

            let hash = {
                use std::hash::BuildHasher;
                std::hash::BuildHasherDefault::<std::hash::DefaultHasher>::default()
                    .hash_one(&self.waves[start..end])
            };
            let mut candidate = self.stored_by_hash.get(&hash).copied();
            while let Some(index) = candidate {
                let stored = &self.stored[index as usize];
                if self.waves[stored.start..stored.end] == self.waves[start..end] {
                    break;
                }
                candidate = (stored.next != NO_CHANGE).then_some(stored.next);
            }
            if let Some(index) = candidate {
                let target = self.stored[index as usize].signal;
                self.positions.push(WavePosition::Alias(u64::from(target)));
                self.waves.truncate(start);
                continue;
            }
            let next = self
                .stored_by_hash
                .insert(hash, self.stored.len() as u32)
                .unwrap_or(NO_CHANGE);
            self.stored.push(StoredWave {
                signal: signal as u32,
                start,
                end,
                next,
            });

            // Offsets are relative to the pack type byte, which comes first.
            let offset = 1 + self.pack.len() as u64;
            self.positions
                .push(WavePosition::Data { offset, length: 0 });
            let wave = &self.waves[start..end];
            if self.options.wave_compression == WaveCompression::Lz4 && wave.len() > 32 {
                self.lz4.clear();
                self.lz4
                    .resize(lz4_flex::block::get_maximum_output_size(wave.len()), 0);
                let table = if wave.len() < u16::MAX as usize {
                    &mut self.lz4_small
                } else {
                    &mut self.lz4_large
                };
                let compressed =
                    lz4_flex::block::compress_into_with_table(wave, &mut self.lz4, table)
                        .expect("output is sized for the worst case");
                if compressed < wave.len() {
                    write_varint(&mut self.pack, wave.len() as u64);
                    self.pack.extend_from_slice(&self.lz4[..compressed]);
                    continue;
                }
            }
            write_varint(&mut self.pack, 0);
            self.pack.extend_from_slice(wave);
        }
        memory_required
    }

    /// [`write_wave`](Self::write_wave) for a 1-bit signal, whose changes all
    /// hold their ASCII byte inline.
    fn write_bit_wave(
        &mut self,
        block: &BlockBuffer,
        signal: usize,
        changes: std::ops::Range<usize>,
    ) {
        let mut previous = self
            .options
            .deduplicate
            .then(|| block.frame[block.layout.frame_offsets[signal]]);
        let mut previous_index = 0u64;
        let order = &self.order[changes];
        for (i, &change) in order.iter().enumerate() {
            if let Some(next) = order.get(i + 1)
                && next.time_index == change.time_index
            {
                continue;
            }
            let ascii = change.data as u8;
            let at_first_step = block.first && change.time_index == 0;
            if let Some(previous) = &mut previous {
                if !at_first_step && ascii == *previous {
                    continue;
                }
                *previous = ascii;
            }
            let time_index = u64::from(change.time_index);
            let delta = time_index - previous_index;
            previous_index = time_index;
            if ascii == b'0' || ascii == b'1' {
                write_varint(&mut self.waves, delta << 2 | u64::from(ascii & 1) << 1);
            } else {
                let code = ONE_BIT_VALUES
                    .iter()
                    .position(|&v| v == ascii)
                    .expect("checked when recorded") as u64;
                write_varint(&mut self.waves, delta << 4 | code << 1 | 1);
            }
        }
    }

    /// Appends the wave of `signal` to `waves`, from the changes at
    /// `order[changes]`.
    fn write_wave(&mut self, block: &BlockBuffer, signal: usize, changes: std::ops::Range<usize>) {
        let length = block.layout.geometry[signal];
        if length == VariableLength::Bits(1) {
            self.write_bit_wave(block, signal, changes);
            return;
        }
        let dedup = self.options.deduplicate && length != VariableLength::VarLen;
        if dedup {
            let offset = block.layout.frame_offsets[signal];
            self.previous.clear();
            match length {
                VariableLength::Bits(bits) => {
                    let ascii = &block.frame[offset..offset + bits as usize];
                    pack_ascii(ascii, &mut self.previous);
                }
                _ => self
                    .previous
                    .extend_from_slice(&block.frame[offset..offset + 8]),
            }
        }

        let mut previous_index = 0u64;
        let order = &self.order[changes];
        for (i, &change) in order.iter().enumerate() {
            if let Some(next) = order.get(i + 1)
                && next.time_index == change.time_index
            {
                continue;
            }
            let packed = wave_value(change, &block.payload, length, &mut self.value);
            let at_first_step = block.first && change.time_index == 0;
            if dedup && !at_first_step && self.value == self.previous {
                continue;
            }

            let time_index = u64::from(change.time_index);
            let delta = time_index - previous_index;
            previous_index = time_index;
            let waves = &mut self.waves;
            match length {
                VariableLength::Bits(_) => {
                    write_varint(waves, delta << 1 | u64::from(!packed));
                    waves.extend_from_slice(&self.value);
                }
                VariableLength::Real => {
                    write_varint(waves, delta << 1 | 1);
                    waves.extend_from_slice(&self.value);
                }
                VariableLength::VarLen => {
                    write_varint(waves, delta << 1);
                    write_varint(waves, self.value.len() as u64);
                    waves.extend_from_slice(&self.value);
                }
            }
            if dedup {
                std::mem::swap(&mut self.value, &mut self.previous);
            }
        }
    }
}

/// Writes the position table of a FST_BL_VCDATA_DYN_ALIAS2 block, the inverse
/// of [`decode_positions_alias2`].
fn write_positions(positions: &[WavePosition], out: &mut Vec<u8>) {
    let mut zeros = 0u64;
    let mut previous_offset = 0u64;
    let mut previous_alias = None;
    for &position in positions {
        let entry = match position {
            WavePosition::None => {
                zeros += 1;
                continue;
            }
            WavePosition::Data { offset, .. } => {
                let delta = (offset - previous_offset) as i64;
                previous_offset = offset;
                delta << 1 | 1
            }
            WavePosition::Alias(id) => {
                let alias = -(id as i64 + 1);
                if previous_alias == Some(alias) {
                    1
                } else {
                    previous_alias = Some(alias);
                    alias << 1 | 1
                }
            }
        };
        if zeros != 0 {
            write_varint(out, zeros << 1);
            zeros = 0;
        }
        write_svarint(out, entry);
    }
    if zeros != 0 {
        write_varint(out, zeros << 1);
    }
}

/// Compresses `data` with zlib at `level` into `scratch` and returns it when
/// strictly smaller: readers take equal lengths to mean the data is raw.
fn zlib_if_smaller<'a>(data: &[u8], level: u32, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
    scratch.clear();
    let mut encoder =
        flate2::write::ZlibEncoder::new(std::mem::take(scratch), flate2::Compression::new(level));
    encoder.write_all(data).ok()?;
    *scratch = encoder.finish().ok()?;
    (scratch.len() < data.len()).then_some(&scratch[..])
}

fn real_to_f64(bytes: [u8; 8], endianness: RealEndianess) -> f64 {
    match endianness {
        RealEndianess::LittleEndian => f64::from_le_bytes(bytes),
        RealEndianess::BigEndian => f64::from_be_bytes(bytes),
    }
}

/// Formats an FST real value the way the reference C library does, with
/// `%.16g`.
pub fn format_real(bytes: [u8; 8], endianness: RealEndianess) -> String {
    format_g(real_to_f64(bytes, endianness), 16)
}

/// A minimal `%.*g` formatter: `precision` significant digits, shortest of
/// fixed and scientific, trailing zeros trimmed.
pub fn format_g(value: f64, precision: usize) -> String {
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

/// Appends `value` as a signed LEB128 varint, the inverse of [`read_svarint`].
fn write_svarint(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
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

fn invalid_input(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
}

/// Decompressed FST payloads at or above this size are staged in a temp file
/// instead of memory, bounding peak RAM for large wrapped dumps.
const WRAPPER_MEMORY_LIMIT: u64 = 256 * 1024 * 1024;

/// A seekable reader over an FST file's bytes, whichever form the input took.
///
/// A plain file reads straight from disk; a [gzip-wrapped](BlockType::GzippedWrapper)
/// file is decompressed once by [`FstInput`] and read back from memory (small)
/// or a temp file (large). All three are [`Read`] + [`Seek`], so the section
/// readers do not know or care which they were handed.
pub enum FstSource {
    File(BufReader<File>),
    Memory(Cursor<Arc<[u8]>>),
    Temp(BufReader<File>),
}

impl Read for FstSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            FstSource::File(r) | FstSource::Temp(r) => r.read(buf),
            FstSource::Memory(r) => r.read(buf),
        }
    }
}

impl Seek for FstSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            FstSource::File(r) | FstSource::Temp(r) => r.seek(pos),
            FstSource::Memory(r) => r.seek(pos),
        }
    }
}

/// A resolved FST input that opens fresh seekable readers over the file's
/// contents, transparently decompressing a whole-file gzip wrapper.
///
/// FST readers seek, and the multi-pass tools reopen their input, so the
/// wrapper is decompressed exactly once here: the inner FST is staged in memory
/// or a temp file, and each [`open`](FstInput::open) hands out a fresh
/// [`FstSource`] over it. An unwrapped file is reopened from its path, so the
/// cost matches reading it directly.
pub enum FstInput {
    Plain(PathBuf),
    Memory(Arc<[u8]>),
    Temp(TempFile),
}

impl FstInput {
    /// Resolves `path`, decompressing it once if it is a gzip wrapper.
    ///
    /// Detection reads only the leading block-type byte; an unwrapped file is
    /// recorded by path and never buffered.
    pub fn open_path(path: &Path) -> std::io::Result<FstInput> {
        let mut head = [0u8; 1];
        let read = File::open(path)?.read(&mut head)?;
        if read == 1 && head[0] == BlockType::GzippedWrapper as u8 {
            Self::unwrap(path)
        } else {
            Ok(FstInput::Plain(path.to_path_buf()))
        }
    }

    fn unwrap(path: &Path) -> std::io::Result<FstInput> {
        let mut file = BufReader::new(File::open(path)?);
        let mut prelude = [0u8; 17];
        file.read_exact(&mut prelude)?;

        let uncompressed_len = u64::from_be_bytes(prelude[9..17].try_into().unwrap());

        if uncompressed_len < WRAPPER_MEMORY_LIMIT {
            let mut buf = Vec::with_capacity(to_usize(uncompressed_len).unwrap_or(0));
            GzDecoder::new(file).read_to_end(&mut buf)?;
            Ok(FstInput::Memory(Arc::from(buf.into_boxed_slice())))
        } else {
            let temp = TempFile::create()?;
            let mut out = BufWriter::new(File::create(temp.path())?);
            std::io::copy(&mut GzDecoder::new(file), &mut out)?;
            out.flush()?;
            drop(out);
            Ok(FstInput::Temp(temp))
        }
    }

    /// Opens a fresh seekable reader over the (possibly decompressed) FST bytes.
    pub fn open(&self) -> std::io::Result<FstSource> {
        match self {
            FstInput::Plain(path) => Ok(FstSource::File(BufReader::new(File::open(path)?))),
            FstInput::Memory(bytes) => Ok(FstSource::Memory(Cursor::new(Arc::clone(bytes)))),
            FstInput::Temp(temp) => Ok(FstSource::Temp(BufReader::new(File::open(temp.path())?))),
        }
    }

    /// Whether the input was a gzip wrapper that had to be decompressed.
    pub fn is_wrapped(&self) -> bool {
        !matches!(self, FstInput::Plain(_))
    }
}

/// A temp file that deletes itself when dropped.
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn create() -> std::io::Result<TempFile> {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "fst-io-{}-{}.fst",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        path.push(unique);
        File::create(&path)?;
        Ok(TempFile { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    // Not `Iterator::next`: this is fallible and each item borrows the iterator,
    // so the trait's signature cannot express it.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> std::io::Result<Option<Section<&mut Take<R>>>> {
        skip_remaining(&mut self.reader)?;
        let reader = self.reader.get_mut();

        let mut kind: u8 = 0;
        match reader.read_exact(std::slice::from_mut(&mut kind)) {
            Ok(_) => (),
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        }

        // A writer leaves the section it is still building tagged Skip, so a
        // Skip tag marks the tail of an interrupted dump. Its length field is
        // not trustworthy; stop the iteration here without reading it.
        if kind == BlockType::Skip as u8 {
            return Ok(None);
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

/// Options of [`FstHierarchyWriter`].
#[derive(Debug, Clone)]
pub struct FstWriterOptions {
    /// Order of magnitude of the time unit: 0 = 1s, -9 = 1ns, etc.
    pub timescale: i8,
    /// `$timezero`: the offset added to every time.
    pub timezero: i64,
    pub file_type: FileType,
    /// The header's simulator identifier, shorter than 128 bytes.
    pub writer: Vec<u8>,
    /// The header's date string, shorter than 26 bytes.
    pub date: Vec<u8>,
    /// A block is sealed at the first time change once this many bytes are
    /// buffered (see [`BlockBuffer::len_bytes`]).
    pub block_size: usize,
    /// Encoder threads. With 0, blocks are encoded and written on the
    /// caller's thread; otherwise one more thread writes them in order. The
    /// file is the same for any count.
    pub threads: usize,
    /// Sealed blocks that may wait to be encoded or written before sealing
    /// another blocks the caller. At least `threads` keeps every encoder
    /// busy.
    pub max_blocks_in_flight: usize,
    pub encode: EncodeOptions,
}

impl Default for FstWriterOptions {
    fn default() -> FstWriterOptions {
        FstWriterOptions {
            timescale: -9,
            timezero: 0,
            file_type: FileType::Verilog,
            writer: b"fst-io".to_vec(),
            date: Vec::new(),
            block_size: 64 << 20,
            threads: 0,
            max_blocks_in_flight: 4,
            encode: EncodeOptions::default(),
        }
    }
}

/// The first phase of writing an FST file: declaring scopes, variables and
/// attributes. [`into_values`](Self::into_values) moves on to the values.
///
/// Dropping the writer finishes the file as one with no value changes,
/// ignoring errors; call [`into_values`](Self::into_values) and
/// [`FstValueWriter::finish`] to see them.
pub struct FstHierarchyWriter<W: Write + Seek + Send + 'static> {
    out: Option<W>,
    options: FstWriterOptions,
    hierarchy: Vec<u8>,
    geometry: Vec<VariableLength>,
    num_scopes: u64,
    num_hierarchy_vars: u64,
    open_scopes: usize,
}

impl<W: Write + Seek + Send + 'static> FstHierarchyWriter<W> {
    /// Starts a file written to `out`. Nothing is written until
    /// [`into_values`](Self::into_values).
    pub fn new(out: W, options: FstWriterOptions) -> std::io::Result<FstHierarchyWriter<W>> {
        if options.writer.len() >= 128 {
            return Err(invalid_input(
                "writer string must be shorter than 128 bytes",
            ));
        }
        if options.date.len() >= 26 {
            return Err(invalid_input("date string must be shorter than 26 bytes"));
        }
        if options.threads > 0 && options.max_blocks_in_flight == 0 {
            return Err(invalid_input("max_blocks_in_flight must be at least 1"));
        }
        Ok(FstHierarchyWriter {
            out: Some(out),
            options,
            hierarchy: Vec::new(),
            geometry: Vec::new(),
            num_scopes: 0,
            num_hierarchy_vars: 0,
            open_scopes: 0,
        })
    }

    /// Enters a new scope, closed by [`upscope`](Self::upscope).
    pub fn scope(
        &mut self,
        scope_type: ScopeType,
        name: &[u8],
        component: &[u8],
    ) -> std::io::Result<()> {
        self.entry(&HierarchyEntry::Scope {
            scope_type: scope_type as u8,
            name: name.to_vec(),
            component: component.to_vec(),
        })
        .map(drop)
    }

    pub fn upscope(&mut self) -> std::io::Result<()> {
        self.entry(&HierarchyEntry::UpScope).map(drop)
    }

    /// Declares a variable in the current scope and returns its handle.
    ///
    /// `length` is in bits. The real types and [`VarType::String`] take their
    /// geometry from the type and ignore it; any other variable of length 0
    /// is variable-length. A [`VarType::Port`] length is stored as given.
    pub fn var(
        &mut self,
        var_type: VarType,
        direction: VarDirection,
        name: &[u8],
        length: u32,
    ) -> std::io::Result<FstSignalHandle> {
        let entry = var_entry(var_type, direction, name, length, None);
        Ok(self.entry(&entry)?.expect("a variable has a handle"))
    }

    /// Declares a variable that shares the values of `of`, and returns `of`.
    pub fn alias(
        &mut self,
        var_type: VarType,
        direction: VarDirection,
        name: &[u8],
        length: u32,
        of: FstSignalHandle,
    ) -> std::io::Result<FstSignalHandle> {
        let entry = var_entry(var_type, direction, name, length, Some(of));
        Ok(self.entry(&entry)?.expect("a variable has a handle"))
    }

    /// Begins an attribute of the current scope. `subtype` is one of the
    /// FST_*_* subtypes of `attr_type`; a [`AttributeType::Misc`] attribute
    /// needs no [`attribute_end`](Self::attribute_end).
    pub fn attribute_begin(
        &mut self,
        attr_type: AttributeType,
        subtype: u8,
        name: &[u8],
        value: u64,
    ) -> std::io::Result<()> {
        self.entry(&HierarchyEntry::AttributeBegin {
            attr_type: attr_type as u8,
            subtype,
            name: name.to_vec(),
            value,
        })
        .map(drop)
    }

    pub fn attribute_end(&mut self) -> std::io::Result<()> {
        self.entry(&HierarchyEntry::AttributeEnd).map(drop)
    }

    /// Writes an entry, such as one read from another file, and returns the
    /// handle of a variable. A non-aliased variable gets the next handle
    /// whatever its `id`; an alias's `id` must be a declared variable.
    pub fn entry(&mut self, entry: &HierarchyEntry) -> std::io::Result<Option<FstSignalHandle>> {
        let mut handle = None;
        match entry {
            HierarchyEntry::AttributeBegin { name, .. } => check_name(name)?,
            HierarchyEntry::AttributeEnd => {}
            HierarchyEntry::Scope {
                name, component, ..
            } => {
                check_name(name)?;
                check_name(component)?;
                self.num_scopes += 1;
                self.open_scopes += 1;
            }
            HierarchyEntry::UpScope => {
                self.open_scopes = self
                    .open_scopes
                    .checked_sub(1)
                    .ok_or_else(|| invalid_input("upscope without an open scope"))?;
            }
            &HierarchyEntry::Var {
                var_type,
                ref name,
                length,
                id,
                is_alias,
                ..
            } => {
                check_name(name)?;
                if VarType::from_u8(var_type).is_none() {
                    return Err(invalid_input("unknown variable type"));
                }
                if is_alias {
                    if id >= self.geometry.len() as u64 {
                        return Err(invalid_input("alias of an undeclared variable"));
                    }
                    handle = Some(FstSignalHandle::from_index(id as usize));
                } else {
                    let geometry = if is_real_var_type(var_type) {
                        VariableLength::Real
                    } else if length == 0 {
                        VariableLength::VarLen
                    } else if length < 0xFFFF_FFFF {
                        VariableLength::Bits(length)
                    } else {
                        return Err(invalid_input("variable length too large"));
                    };
                    if self.geometry.len() >= (u32::MAX - 1) as usize {
                        return Err(invalid_input("too many variables"));
                    }
                    self.geometry.push(geometry);
                    handle = Some(FstSignalHandle::from_index(self.geometry.len() - 1));
                }
                self.num_hierarchy_vars += 1;
            }
        }
        entry.write_record(&mut self.hierarchy);
        Ok(handle)
    }

    /// Writes the header, hierarchy and geometry, and moves on to the values.
    pub fn into_values(mut self) -> std::io::Result<FstValueWriter<W>> {
        self.start_values()
    }

    fn start_values(&mut self) -> std::io::Result<FstValueWriter<W>> {
        let mut out = self.out.take().expect("only taken once");
        let header = self.header();
        write_section_header(&mut out, BlockType::Header as u8, HeaderBlock::LEN as u64)?;
        out.write_all(&header.to_bytes())?;

        let mut section = Vec::new();
        encode_hierarchy_section(&self.hierarchy, &mut section);
        out.write_all(&section)?;
        encode_geometry_section(&self.geometry, &mut section);
        out.write_all(&section)?;

        FstValueWriter::new(out, header, &self.geometry, &self.options)
    }

    fn header(&self) -> HeaderBlock {
        let mut writer = [0u8; 128];
        writer[..self.options.writer.len()].copy_from_slice(&self.options.writer);
        let mut date = [0u8; 26];
        date[..self.options.date.len()].copy_from_slice(&self.options.date);
        HeaderBlock {
            start_time: 0,
            end_time: 0,
            real_endianness: RealEndianess::LittleEndian,
            writer_memory_use: self.options.block_size as u64,
            num_scopes: self.num_scopes,
            num_hierarchy_vars: self.num_hierarchy_vars,
            num_vars: self.geometry.len() as u64,
            num_vc_blocks: 0,
            timescale: self.options.timescale,
            writer,
            date,
            reserved: [0; 93],
            file_type: self.options.file_type as u8,
            timezero: self.options.timezero,
        }
    }
}

impl<W: Write + Seek + Send + 'static> Drop for FstHierarchyWriter<W> {
    fn drop(&mut self) {
        if self.out.is_some()
            && let Ok(values) = self.start_values()
        {
            let _ = values.finish();
        }
    }
}

/// The hierarchy record of a variable declared through
/// [`FstHierarchyWriter::var`] or [`FstHierarchyWriter::alias`]: reals are
/// recorded with length 8 and strings with length 0, as libfst does.
fn var_entry(
    var_type: VarType,
    direction: VarDirection,
    name: &[u8],
    length: u32,
    alias_of: Option<FstSignalHandle>,
) -> HierarchyEntry {
    let length = if is_real_var_type(var_type as u8) {
        8
    } else if var_type == VarType::String {
        0
    } else {
        u64::from(length)
    };
    HierarchyEntry::Var {
        var_type: var_type as u8,
        direction: direction as u8,
        name: name.to_vec(),
        length,
        id: alias_of.map_or(0, |of| of.index() as u64),
        is_alias: alias_of.is_some(),
    }
}

fn check_name(name: &[u8]) -> std::io::Result<()> {
    if name.contains(&0) {
        return Err(invalid_input("name contains a NUL byte"));
    }
    Ok(())
}

/// Encodes a FST_BL_HIER_LZ4 section holding `records` into `out`,
/// replacing its contents.
fn encode_hierarchy_section(records: &[u8], out: &mut Vec<u8>) {
    let mut compressed = vec![0; lz4_flex::block::get_maximum_output_size(records.len())];
    let len = lz4_flex::block::compress_into(records, &mut compressed)
        .expect("output is sized for the worst case");
    out.clear();
    write_section_header(out, BlockType::HierarchyLZ4 as u8, 8 + len as u64)
        .expect("writing to a Vec does not fail");
    out.extend_from_slice(&(records.len() as u64).to_be_bytes());
    out.extend_from_slice(&compressed[..len]);
}

/// Encodes a geometry section into `out`, replacing its contents.
fn encode_geometry_section(geometry: &[VariableLength], out: &mut Vec<u8>) {
    let mut lengths = Vec::new();
    for length in geometry {
        write_varint(&mut lengths, length.to_geometry());
    }
    let mut scratch = Vec::new();
    let data = zlib_if_smaller(&lengths, 9, &mut scratch).unwrap_or(&lengths);
    out.clear();
    write_section_header(out, BlockType::Geometry as u8, 16 + data.len() as u64)
        .expect("writing to a Vec does not fail");
    out.extend_from_slice(&(lengths.len() as u64).to_be_bytes());
    out.extend_from_slice(&(geometry.len() as u64).to_be_bytes());
    out.extend_from_slice(data);
}

/// Encodes a blackout section, the `$dumpoff`/`$dumpon` times, into `out`,
/// replacing its contents.
fn encode_blackout_section(blackouts: &[(u64, bool)], out: &mut Vec<u8>) {
    let mut data = Vec::new();
    write_varint(&mut data, blackouts.len() as u64);
    let mut previous = 0;
    for &(time, active) in blackouts {
        data.push(u8::from(active));
        write_varint(&mut data, time - previous);
        previous = time;
    }
    out.clear();
    write_section_header(out, BlockType::Blackout as u8, data.len() as u64)
        .expect("writing to a Vec does not fail");
    out.extend_from_slice(&data);
}

/// The second phase of writing an FST file: the value changes.
///
/// Changes are recorded at the current time, set by [`time`](Self::time);
/// changes made before the first `time` call happen at the first time. Each
/// change is checked against the signal's geometry and returns
/// [`std::io::ErrorKind::InvalidInput`] when it does not fit. Blocks are
/// sealed at a time change once [`FstWriterOptions::block_size`] bytes are
/// buffered or after [`flush`](Self::flush).
///
/// Dropping the writer finishes the file, ignoring errors; call
/// [`finish`](Self::finish) to see them.
pub struct FstValueWriter<W: Write + Seek + Send + 'static> {
    output: Option<Output<W>>,
    header: HeaderBlock,
    block: BlockBuffer,
    block_size: usize,
    flush_pending: bool,
    blackouts: Vec<(u64, bool)>,
}

impl<W: Write + Seek + Send + 'static> FstValueWriter<W> {
    fn new(
        out: W,
        header: HeaderBlock,
        geometry: &[VariableLength],
        options: &FstWriterOptions,
    ) -> std::io::Result<FstValueWriter<W>> {
        let output = if options.threads == 0 {
            Output::Inline {
                out,
                encoder: Box::new(BlockEncoder::new(options.encode)),
                section: Vec::new(),
                spare: None,
            }
        } else {
            Output::Threads(Pipeline::new(
                out,
                options.threads,
                options.max_blocks_in_flight,
                options.encode,
            )?)
        };
        Ok(FstValueWriter {
            output: Some(output),
            header,
            block: BlockBuffer::new(geometry)?,
            block_size: options.block_size,
            flush_pending: false,
            blackouts: Vec::new(),
        })
    }

    /// Advances to `time`. Going back in time is an error; repeating the
    /// current time does nothing.
    pub fn time(&mut self, time: u64) -> std::io::Result<()> {
        match self.block.end_time() {
            Some(last) if time < last => return Err(invalid_input("time goes backwards")),
            Some(last) if time == last => return Ok(()),
            Some(_) => {
                let full = self.block.len_bytes() >= self.block_size;
                if (self.flush_pending || full) && self.block.has_time_step() {
                    self.seal()?;
                }
            }
            None => {
                self.header.start_time = time;
                for blackout in &mut self.blackouts {
                    blackout.0 = time;
                }
            }
        }
        self.block.push_time(time)
    }

    /// Records a value of any form the reader yields, so values read from
    /// one file can be written to another as they are. [`Value::Real`] bytes
    /// are written as they are, so they must be little-endian.
    pub fn change(&mut self, handle: FstSignalHandle, value: Value<'_>) -> std::io::Result<()> {
        self.block.change(handle, value)
    }

    /// Records a value of a 1-bit signal.
    pub fn change_bit(&mut self, handle: FstSignalHandle, bit: bool) -> std::io::Result<()> {
        self.block.change_bit(handle, bit)
    }

    /// Records the low `n` bits of `value`, for an n-bit signal with
    /// `n <= 64`.
    pub fn change_u64(&mut self, handle: FstSignalHandle, value: u64) -> std::io::Result<()> {
        self.block.change_u64(handle, value)
    }

    /// See [`BlockBuffer::change_words32`].
    pub fn change_words32(
        &mut self,
        handle: FstSignalHandle,
        words: &[u32],
    ) -> std::io::Result<()> {
        self.block.change_words32(handle, words)
    }

    /// See [`BlockBuffer::change_words64`].
    pub fn change_words64(
        &mut self,
        handle: FstSignalHandle,
        words: &[u64],
    ) -> std::io::Result<()> {
        self.block.change_words64(handle, words)
    }

    /// See [`BlockBuffer::change_four_state`].
    pub fn change_four_state(
        &mut self,
        handle: FstSignalHandle,
        aval: &[u32],
        bval: &[u32],
    ) -> std::io::Result<()> {
        self.block.change_four_state(handle, aval, bval)
    }

    pub fn change_f64(&mut self, handle: FstSignalHandle, value: f64) -> std::io::Result<()> {
        self.block.change_f64(handle, value)
    }

    /// See [`BlockBuffer::change_ascii`].
    pub fn change_ascii(&mut self, handle: FstSignalHandle, ascii: &[u8]) -> std::io::Result<()> {
        self.block.change_ascii(handle, ascii)
    }

    pub fn change_var_len(&mut self, handle: FstSignalHandle, bytes: &[u8]) -> std::io::Result<()> {
        self.block.change_var_len(handle, bytes)
    }

    /// Records a `$dumpoff` (`active == false`) or `$dumpon` at the current
    /// time.
    pub fn dump_active(&mut self, active: bool) {
        let time = self.block.end_time().unwrap_or(0);
        self.blackouts.push((time, active));
    }

    /// Advances through several time steps at once and returns a
    /// [`ColumnBlock`] to record their changes signal by signal, the way the
    /// file stores them.
    ///
    /// `times` must be increasing and start at or after the current time; a
    /// first time equal to the current time continues the current time step.
    /// Moving to `times[0]` may seal the current block, as any time change
    /// does; the rest of `times` then lands in one block. Changes recorded
    /// through [`time`](Self::time) and `change*` afterwards happen at the
    /// last of `times`.
    pub fn block(&mut self, times: &[u64]) -> std::io::Result<ColumnBlock<'_, W>> {
        if times.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid_input("times must be increasing"));
        }
        let Some((&first, rest)) = times.split_first() else {
            return Ok(ColumnBlock {
                writer: self,
                base: 0,
                len: 0,
            });
        };
        self.time(first)?;
        let base = self.block.times().len() - 1;
        for &time in rest {
            self.block.push_time(time)?;
        }
        Ok(ColumnBlock {
            writer: self,
            base,
            len: times.len(),
        })
    }

    /// Seals the current block at the next time change. Returns an error an
    /// encoder or writer thread met since the last call.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.flush_pending = true;
        match &self.output {
            Some(Output::Threads(pipeline)) => pipeline.shared.check(),
            _ => Ok(()),
        }
    }

    /// Writes the last block and the blackout section, waits for every block
    /// to be written, rewrites the header, and returns the output.
    pub fn finish(mut self) -> std::io::Result<W> {
        self.finish_file()
    }

    fn seal(&mut self) -> std::io::Result<()> {
        self.flush_pending = false;
        let output = self.output.as_mut().expect("present until finished");
        output.seal(&mut self.block)?;
        self.header.num_vc_blocks += 1;
        Ok(())
    }

    fn finish_file(&mut self) -> std::io::Result<W> {
        if self.block.end_time().is_none() {
            self.time(0)?;
        }
        let output = self.output.take().expect("present until finished");
        self.header.end_time = self.block.end_time().expect("a time was pushed");
        let last = if self.block.has_time_step() {
            self.header.num_vc_blocks += 1;
            Some(std::mem::replace(
                &mut self.block,
                BlockBuffer::placeholder(),
            ))
        } else {
            None
        };
        let mut out = output.finish(last)?;

        if !self.blackouts.is_empty() {
            let mut section = Vec::new();
            encode_blackout_section(&self.blackouts, &mut section);
            out.write_all(&section)?;
        }
        let end = out.stream_position()?;
        out.seek(SeekFrom::Start(9))?;
        out.write_all(&self.header.to_bytes())?;
        out.seek(SeekFrom::Start(end))?;
        out.flush()?;
        Ok(out)
    }
}

impl<W: Write + Seek + Send + 'static> Drop for FstValueWriter<W> {
    fn drop(&mut self) {
        if self.output.is_some() {
            let _ = self.finish_file();
        }
    }
}

/// Time steps added by [`FstValueWriter::block`], whose changes are given per
/// signal.
pub struct ColumnBlock<'a, W: Write + Seek + Send + 'static> {
    writer: &'a mut FstValueWriter<W>,
    /// Index in the block's time table of the first of these time steps.
    base: usize,
    len: usize,
}

impl<W: Write + Seek + Send + 'static> ColumnBlock<'_, W> {
    /// Records changes of one signal as (index into the `times` given to
    /// [`FstValueWriter::block`], value), in time order. A signal may be
    /// given again with later changes; a later change at the same index
    /// replaces the earlier one. A rejected change leaves the ones before it
    /// recorded.
    pub fn signal<'v>(
        &mut self,
        handle: FstSignalHandle,
        changes: impl IntoIterator<Item = (usize, Value<'v>)>,
    ) -> std::io::Result<()> {
        for (index, value) in changes {
            if index >= self.len {
                return Err(invalid_input("time index out of range"));
            }
            self.writer
                .block
                .change_at(self.base + index, handle, value)?;
        }
        Ok(())
    }
}

/// Where sealed blocks go: encoded and written on the caller's thread, or
/// handed to a [`Pipeline`].
enum Output<W> {
    Inline {
        out: W,
        encoder: Box<BlockEncoder>,
        section: Vec<u8>,
        spare: Option<BlockBuffer>,
    },
    Threads(Pipeline<W>),
}

impl<W: Write + Send + 'static> Output<W> {
    /// Replaces `block` by the block that follows it, and encodes and writes
    /// `block`.
    fn seal(&mut self, block: &mut BlockBuffer) -> std::io::Result<()> {
        match self {
            Output::Inline {
                out,
                encoder,
                section,
                spare,
            } => {
                let mut next = spare.take().unwrap_or_else(BlockBuffer::placeholder);
                block.start_next(&mut next)?;
                let sealed = std::mem::replace(block, next);
                let result = encoder
                    .encode(&sealed, section)
                    .and_then(|()| out.write_all(section));
                *spare = Some(sealed);
                result
            }
            Output::Threads(pipeline) => {
                let mut next = pipeline.reserve()?;
                if let Err(err) = block.start_next(&mut next) {
                    pipeline.release(next);
                    return Err(err);
                }
                let sealed = std::mem::replace(block, next);
                pipeline.send(sealed)
            }
        }
    }

    /// Encodes and writes `last`, waits for every block to be written, and
    /// returns the output.
    fn finish(self, last: Option<BlockBuffer>) -> std::io::Result<W> {
        match self {
            Output::Inline {
                mut out,
                mut encoder,
                mut section,
                ..
            } => {
                if let Some(last) = last {
                    encoder.encode(&last, &mut section)?;
                    out.write_all(&section)?;
                }
                Ok(out)
            }
            Output::Threads(pipeline) => pipeline.finish(last),
        }
    }
}

/// A pool of encoder threads and a writer thread. Sealed blocks are numbered
/// in order; the encoders take them in any order, and the writer puts the
/// sections back in order before writing them.
struct Pipeline<W> {
    jobs: Option<std::sync::mpsc::Sender<(u64, BlockBuffer)>>,
    shared: Arc<PipelineShared>,
    encoders: Vec<std::thread::JoinHandle<()>>,
    writer: std::thread::JoinHandle<Option<W>>,
    next_sequence: u64,
}

struct PipelineShared {
    state: std::sync::Mutex<PipelineState>,
    /// Signalled when a block is written or an error is stored.
    changed: std::sync::Condvar,
    max_in_flight: usize,
}

struct PipelineState {
    /// Blocks sealed and not yet written.
    in_flight: usize,
    /// The first error of an encoder or writer thread.
    error: Option<std::io::Error>,
    free_blocks: Vec<BlockBuffer>,
    free_sections: Vec<Vec<u8>>,
}

impl PipelineShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, PipelineState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fail(&self, error: std::io::Error) {
        let mut state = self.lock();
        state.error.get_or_insert(error);
        drop(state);
        self.changed.notify_all();
    }

    fn check(&self) -> std::io::Result<()> {
        match &self.lock().error {
            Some(error) => Err(copy_error(error)),
            None => Ok(()),
        }
    }
}

fn copy_error(error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), error.to_string())
}

/// Stores an error when a pipeline thread panics, so the caller does not
/// wait for a block that will never be written.
struct PanicGuard<'a>(&'a PipelineShared);

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0
                .fail(std::io::Error::other("an FST writer thread panicked"));
        }
    }
}

impl<W: Write + Send + 'static> Pipeline<W> {
    fn new(
        out: W,
        threads: usize,
        max_in_flight: usize,
        options: EncodeOptions,
    ) -> std::io::Result<Pipeline<W>> {
        let shared = Arc::new(PipelineShared {
            state: std::sync::Mutex::new(PipelineState {
                in_flight: 0,
                error: None,
                free_blocks: Vec::new(),
                free_sections: Vec::new(),
            }),
            changed: std::sync::Condvar::new(),
            max_in_flight,
        });
        let (jobs, job_receiver) = std::sync::mpsc::channel();
        let job_receiver = Arc::new(std::sync::Mutex::new(job_receiver));
        let (sections, section_receiver) = std::sync::mpsc::channel();

        let mut encoders = Vec::with_capacity(threads);
        for i in 0..threads {
            let job_receiver = Arc::clone(&job_receiver);
            let sections = sections.clone();
            let shared = Arc::clone(&shared);
            let encoder = std::thread::Builder::new()
                .name(format!("fst-encode-{i}"))
                .spawn(move || encode_blocks(&job_receiver, &sections, &shared, options))?;
            encoders.push(encoder);
        }
        drop(sections);
        let writer_shared = Arc::clone(&shared);
        let writer = std::thread::Builder::new()
            .name("fst-write".to_string())
            .spawn(move || write_sections(out, section_receiver, &writer_shared))?;

        Ok(Pipeline {
            jobs: Some(jobs),
            shared,
            encoders,
            writer,
            next_sequence: 0,
        })
    }

    /// Waits for room for one more sealed block, and returns a block buffer
    /// to fill next.
    fn reserve(&mut self) -> std::io::Result<BlockBuffer> {
        let mut state = self.shared.lock();
        while state.in_flight >= self.shared.max_in_flight && state.error.is_none() {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if let Some(error) = &state.error {
            return Err(copy_error(error));
        }
        state.in_flight += 1;
        Ok(state
            .free_blocks
            .pop()
            .unwrap_or_else(BlockBuffer::placeholder))
    }

    /// Gives back a block from [`reserve`](Self::reserve) that was not sent.
    fn release(&mut self, block: BlockBuffer) {
        let mut state = self.shared.lock();
        state.in_flight -= 1;
        state.free_blocks.push(block);
    }

    fn send(&mut self, block: BlockBuffer) -> std::io::Result<()> {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let jobs = self.jobs.as_ref().expect("present until finished");
        if jobs.send((sequence, block)).is_err() {
            self.shared.check()?;
            return Err(std::io::Error::other("FST encoder threads stopped"));
        }
        Ok(())
    }

    fn finish(mut self, last: Option<BlockBuffer>) -> std::io::Result<W> {
        let sent = match last {
            Some(last) => self.reserve().and_then(|spare| {
                self.shared.lock().free_blocks.push(spare);
                self.send(last)
            }),
            None => Ok(()),
        };
        drop(self.jobs.take());
        for encoder in self.encoders.drain(..) {
            let _ = encoder.join();
        }
        let out = self.writer.join();
        self.shared.check()?;
        sent?;
        match out {
            Ok(Some(out)) => Ok(out),
            _ => Err(std::io::Error::other("FST writer thread stopped")),
        }
    }
}

fn encode_blocks(
    jobs: &std::sync::Mutex<std::sync::mpsc::Receiver<(u64, BlockBuffer)>>,
    sections: &std::sync::mpsc::Sender<(u64, Vec<u8>)>,
    shared: &PipelineShared,
    options: EncodeOptions,
) {
    let _guard = PanicGuard(shared);
    let mut encoder = BlockEncoder::new(options);
    loop {
        let job = jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        let Ok((sequence, block)) = job else {
            return;
        };
        let mut section = shared.lock().free_sections.pop().unwrap_or_default();
        let result = encoder.encode(&block, &mut section);
        shared.lock().free_blocks.push(block);
        match result {
            Ok(()) => {
                if sections.send((sequence, section)).is_err() {
                    return;
                }
            }
            Err(error) => {
                shared.fail(error);
                return;
            }
        }
    }
}

/// Writes the encoded sections in sequence order, and returns `out`, or
/// `None` after an error.
fn write_sections<W: Write>(
    mut out: W,
    sections: std::sync::mpsc::Receiver<(u64, Vec<u8>)>,
    shared: &PipelineShared,
) -> Option<W> {
    let _guard = PanicGuard(shared);
    let mut pending = std::collections::BTreeMap::new();
    let mut next = 0u64;
    for (sequence, section) in sections {
        pending.insert(sequence, section);
        while let Some(section) = pending.remove(&next) {
            if let Err(error) = out.write_all(&section) {
                shared.fail(error);
                return None;
            }
            next += 1;
            let mut state = shared.lock();
            state.in_flight -= 1;
            state.free_sections.push(section);
            drop(state);
            shared.changed.notify_all();
        }
    }
    Some(out)
}
