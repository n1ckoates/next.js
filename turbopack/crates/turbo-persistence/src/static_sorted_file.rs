use std::{
    cmp::Ordering,
    fs::File,
    hash::BuildHasherDefault,
    mem::{transmute, MaybeUninit},
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use anyhow::{bail, Result};
use byteorder::{ReadBytesExt, BE};
use lzzzz::lz4::decompress_with_dict;
use memmap2::Mmap;
use quick_cache::sync::GuardResult;
use rustc_hash::FxHasher;

use crate::arc_slice::ArcSlice;

pub const BLOCK_TYPE_INDEX: u8 = 0;
pub const BLOCK_TYPE_KEY: u8 = 1;

pub const KEY_BLOCK_ENTRY_TYPE_NORMAL: u8 = 0;
pub const KEY_BLOCK_ENTRY_TYPE_BLOB: u8 = 1;
pub const KEY_BLOCK_ENTRY_TYPE_DELETED: u8 = 2;

pub enum LookupResult {
    Deleted,
    Small { value: ArcSlice<u8> },
    Blob { sequence_number: u32 },
    QuickFilterMiss,
    RangeMiss,
    KeyMiss,
}

struct LocationInFile {
    start: usize,
    end: usize,
}

struct Header {
    aqmf: LocationInFile,
    key_compression_dictionary: LocationInFile,
    value_compression_dictionary: LocationInFile,
    block_offsets_start: usize,
    blocks_start: usize,
}

#[derive(Clone, Default)]
pub struct AqmfWeighter;

impl quick_cache::Weighter<u32, Arc<qfilter::Filter>> for AqmfWeighter {
    fn weight(&self, _key: &u32, filter: &Arc<qfilter::Filter>) -> u64 {
        filter.capacity() as u64 + 1
    }
}

#[derive(Clone, Default)]
pub struct BlockWeighter;

impl quick_cache::Weighter<(u32, u8), ArcSlice<u8>> for BlockWeighter {
    fn weight(&self, _key: &(u32, u8), val: &ArcSlice<u8>) -> u64 {
        val.len() as u64 + 8
    }
}

pub type AqmfCache =
    quick_cache::sync::Cache<u32, Arc<qfilter::Filter>, AqmfWeighter, BuildHasherDefault<FxHasher>>;
pub type BlockCache =
    quick_cache::sync::Cache<(u32, u8), ArcSlice<u8>, BlockWeighter, BuildHasherDefault<FxHasher>>;

pub struct StaticSortedFile {
    sequence_number: u32,
    mmap: Mmap,
    header: OnceLock<Header>,
}

impl StaticSortedFile {
    pub fn open(sequence_number: u32, path: PathBuf) -> Result<Self> {
        let mmap = unsafe { Mmap::map(&File::open(&path)?)? };
        let file = Self {
            sequence_number,
            mmap,
            header: OnceLock::new(),
        };
        Ok(file)
    }

    fn header(&self) -> Result<&Header> {
        self.header.get_or_try_init(|| {
            let mut file = &*self.mmap;
            let magic = file.read_u32::<BE>()?;
            if magic != 0x53535401 {
                bail!("Invalid magic number or version");
            }
            let aqmf_length = file.read_u24::<BE>()? as usize;
            let key_compression_dictionary_length = file.read_u16::<BE>()? as usize;
            let value_compression_dictionary_length = file.read_u16::<BE>()? as usize;
            let block_count = file.read_u8()? as usize;
            const HEADER_SIZE: usize = 12;
            let mut current_offset = HEADER_SIZE;
            let aqmf = LocationInFile {
                start: current_offset,
                end: current_offset + aqmf_length,
            };
            current_offset += aqmf_length;
            let key_compression_dictionary = LocationInFile {
                start: current_offset,
                end: current_offset + key_compression_dictionary_length,
            };
            current_offset += key_compression_dictionary_length;
            let value_compression_dictionary = LocationInFile {
                start: current_offset,
                end: current_offset + value_compression_dictionary_length,
            };
            current_offset += value_compression_dictionary_length;
            let block_offsets_start = current_offset;
            let blocks_start = block_offsets_start + block_count * 4;

            Ok(Header {
                aqmf,
                key_compression_dictionary,
                value_compression_dictionary,
                block_offsets_start,
                blocks_start,
            })
        })
    }

    pub fn lookup(
        &self,
        key: &[u8],
        aqmf_cache: &AqmfCache,
        key_block_cache: &BlockCache,
        value_block_cache: &BlockCache,
    ) -> Result<LookupResult> {
        let aqmf = match aqmf_cache.get_value_or_guard(&self.sequence_number, None) {
            GuardResult::Value(aqmf) => aqmf,
            GuardResult::Guard(guard) => {
                let header = self.header()?;
                let aqmf = &self.mmap[header.aqmf.start..header.aqmf.end];
                let aqmf: Arc<qfilter::Filter> = Arc::new(pot::from_slice(aqmf)?);
                let _ = guard.insert(aqmf.clone());
                aqmf
            }
            GuardResult::Timeout => unreachable!(),
        };
        if !aqmf.contains(key) {
            return Ok(LookupResult::QuickFilterMiss);
        }
        let header = self.header()?;
        let mut current_block = 0;
        loop {
            let block = match key_block_cache
                .get_value_or_guard(&(self.sequence_number, current_block), None)
            {
                GuardResult::Value(block) => block,
                GuardResult::Guard(guard) => {
                    let block = self.read_key_block(header, current_block)?;
                    let _ = guard.insert(block.clone());
                    block
                }
                GuardResult::Timeout => unreachable!(),
            };
            let mut block = &block[..];
            let block_type = block.read_u8()?;
            match block_type {
                BLOCK_TYPE_INDEX => {
                    if let Some(next_block) = self.lookup_index_block(block, key)? {
                        current_block = next_block;
                    } else {
                        return Ok(LookupResult::RangeMiss);
                    }
                }
                BLOCK_TYPE_KEY => {
                    return self.lookup_key_block(block, key, header, value_block_cache);
                }
                _ => {
                    bail!("Invalid block type");
                }
            }
        }
    }

    fn lookup_index_block(&self, mut block: &[u8], key: &[u8]) -> Result<Option<u8>> {
        let entry_count = block.read_u16::<BE>()? as usize;
        let start_entries = (entry_count - 1) * 2;
        let offsets = &block[..start_entries];
        let entries = &block[start_entries..];
        fn get_key<'l>(
            offsets: &[u8],
            entries: &'l [u8],
            entry_count: usize,
            index: usize,
        ) -> Result<&'l [u8]> {
            let start = if index == 0 {
                0
            } else {
                (&offsets[(index - 1) * 2..]).read_u16::<BE>()? as usize
            };
            let end = if index == entry_count - 1 {
                entries.len()
            } else {
                (&offsets[index * 2..]).read_u16::<BE>()? as usize - 1
            };
            Ok(&entries[start..end])
        }
        fn get_block(offsets: &[u8], entries: &[u8], index: usize) -> Result<u8> {
            Ok(entries[(&offsets[index * 2..]).read_u16::<BE>()? as usize - 1])
        }
        let left_key = get_key(&offsets, &entries, entry_count, 0)?;
        match key.cmp(left_key) {
            Ordering::Less => {
                // not in this block
                return Ok(None);
            }
            Ordering::Equal => {
                // It's in the first range
                return Ok(Some(get_block(&offsets, &entries, 0)?));
            }
            Ordering::Greater => {}
        }
        let right_key = get_key(&offsets, &entries, entry_count, entry_count as usize - 1)?;
        match right_key.cmp(key) {
            Ordering::Less => {
                // not in this block
                return Ok(None);
            }
            Ordering::Equal => {
                // It's in the last range
                return Ok(Some(get_block(
                    &offsets,
                    &entries,
                    entry_count as usize - 2,
                )?));
            }
            Ordering::Greater => {}
        }
        let mut l = 0;
        let mut r = entry_count;
        // binary search for the range
        while l < r {
            let m = (l + r) / 2;
            let mid_key = get_key(&offsets, &entries, entry_count, m)?;
            match key.cmp(mid_key) {
                Ordering::Less => {
                    r = m;
                }
                Ordering::Equal => {
                    return Ok(Some(get_block(&offsets, &entries, m - 1)?));
                }
                Ordering::Greater => {
                    l = m + 1;
                }
            }
        }
        Ok(Some(get_block(&offsets, &entries, l - 1)?))
    }

    fn lookup_key_block(
        &self,
        mut block: &[u8],
        key: &[u8],
        header: &Header,
        value_block_cache: &BlockCache,
    ) -> Result<LookupResult> {
        let entry_count = block.read_u24::<BE>()? as usize;
        let offsets = &block[..entry_count * 4];
        let entries = &block[entry_count * 4..];
        fn get_entry<'l>(
            offsets: &[u8],
            entries: &'l [u8],
            entry_count: usize,
            index: usize,
        ) -> Result<(&'l [u8], u8, &'l [u8])> {
            let mut offset = &offsets[index * 4..];
            let ty = offset.read_u8()?;
            let start = offset.read_u24::<BE>()? as usize;
            let end = if index == entry_count - 1 {
                entries.len()
            } else {
                (&offsets[(index + 1) * 4 + 1..]).read_u24::<BE>()? as usize
            };
            Ok(match ty {
                KEY_BLOCK_ENTRY_TYPE_NORMAL => {
                    (&entries[start..end - 8], ty, &entries[end - 8..end])
                }
                KEY_BLOCK_ENTRY_TYPE_BLOB => (&entries[start..end - 4], ty, &entries[end - 4..end]),
                KEY_BLOCK_ENTRY_TYPE_DELETED => (&entries[start..end], ty, &entries[start..end]),
                _ => {
                    bail!("Invalid key block entry type");
                }
            })
        }
        let mut l = 0;
        let mut r = entry_count;
        // binary search for the key
        while l < r {
            let m = (l + r) / 2;
            let (mid_key, ty, mid_val) = get_entry(&offsets, &entries, entry_count, m)?;
            match key.cmp(mid_key) {
                Ordering::Less => {
                    r = m;
                }
                Ordering::Equal => {
                    return self.handle_key_match(ty, mid_val, header, value_block_cache);
                }
                Ordering::Greater => {
                    l = m + 1;
                }
            }
        }
        Ok(LookupResult::KeyMiss)
    }

    fn handle_key_match(
        &self,
        ty: u8,
        mut val: &[u8],
        header: &Header,
        value_block_cache: &BlockCache,
    ) -> Result<LookupResult> {
        Ok(match ty {
            KEY_BLOCK_ENTRY_TYPE_NORMAL => {
                let block = val.read_u8()?;
                let size = val.read_u24::<BE>()? as usize;
                let position = val.read_u32::<BE>()? as usize;
                let value =
                    self.lookup_value_block(block, position, size, header, value_block_cache)?;
                LookupResult::Small { value }
            }
            KEY_BLOCK_ENTRY_TYPE_BLOB => {
                let sequence_number = val.read_u32::<BE>()?;
                LookupResult::Blob { sequence_number }
            }
            KEY_BLOCK_ENTRY_TYPE_DELETED => LookupResult::Deleted,
            _ => {
                bail!("Invalid key block entry type");
            }
        })
    }

    fn lookup_value_block(
        &self,
        block: u8,
        position: usize,
        size: usize,
        header: &Header,
        value_block_cache: &BlockCache,
    ) -> Result<ArcSlice<u8>> {
        let block = match value_block_cache.get_value_or_guard(&(self.sequence_number, block), None)
        {
            GuardResult::Value(block) => block,
            GuardResult::Guard(guard) => {
                let block = self.read_value_block(header, block)?;
                let _ = guard.insert(block.clone());
                block
            }
            GuardResult::Timeout => unreachable!(),
        };
        let value = block.slice(position..position + size);
        Ok(value)
    }

    fn read_key_block(&self, header: &Header, block_index: u8) -> Result<ArcSlice<u8>> {
        self.read_block(
            header,
            block_index,
            &self.mmap
                [header.key_compression_dictionary.start..header.key_compression_dictionary.end],
        )
    }

    fn read_value_block(&self, header: &Header, block_index: u8) -> Result<ArcSlice<u8>> {
        self.read_block(
            header,
            block_index,
            &self.mmap[header.value_compression_dictionary.start
                ..header.value_compression_dictionary.end],
        )
    }

    fn read_block(
        &self,
        header: &Header,
        block_index: u8,
        compression_dictionary: &[u8],
    ) -> Result<ArcSlice<u8>> {
        let offset = header.block_offsets_start + block_index as usize * 4;
        let block_start = if block_index == 0 {
            header.blocks_start
        } else {
            header.blocks_start + (&self.mmap[offset - 4..offset]).read_u32::<BE>()? as usize
        };
        let block_end =
            header.blocks_start + (&self.mmap[offset..offset + 4]).read_u32::<BE>()? as usize;
        let uncompressed_length =
            (&self.mmap[block_start..block_start + 4]).read_u32::<BE>()? as usize;
        let block = self.mmap[block_start + 4..block_end].to_vec();

        let buffer = Arc::new_zeroed_slice(uncompressed_length);
        // Safety: MaybeUninit<u8> can be safely transmuted to u8.
        let mut buffer = unsafe { transmute::<Arc<[MaybeUninit<u8>]>, Arc<[u8]>>(buffer) };
        // Safety: We know that the buffer is not shared yet.
        let decompressed = unsafe { Arc::get_mut_unchecked(&mut buffer) };
        decompress_with_dict(&block, decompressed, compression_dictionary)?;
        Ok(ArcSlice::from(buffer))
    }
}
