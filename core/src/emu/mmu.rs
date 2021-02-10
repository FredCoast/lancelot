use std::collections::BTreeMap;

use anyhow::Result;
use bitflags::*;
use bitvec::{prelude::*, vec::BitVec};
use byteorder::{ByteOrder, LittleEndian};
use thiserror::Error;

use crate::{module::Permissions, VA};

pub const PAGE_SIZE: usize = 0x1000;
const PAGE_SHIFT: usize = 12;
const PAGE_MASK: u64 = 0xFFF;
type PageFrame = [u8; PAGE_SIZE];
const EMPTY_PAGE: PageFrame = [0u8; PAGE_SIZE];

/// Page Frame Number
/// "4 billion pages ought to be enough for everyone!"
/// u32 <= usize for all 32- and 64-bit systems,
/// so you'll some casting to/from usize.
type PFN = u32;
/// use to flag PFNs that should not be used an indices.
const INVALID_PFN: PFN = u32::MAX;

/// A collection of "physical" pages of memory, indexed by `PFN`.
#[derive(Clone)]
struct PageFrames {
    // page frames indexed by `PFN`.
    frames:            Vec<PageFrame>,
    // allocation status, indexed by `PFN`.
    // when `true`, the page is allocated.
    allocation_bitmap: BitVec,
}

impl Default for PageFrames {
    fn default() -> Self {
        PageFrames {
            frames:            vec![],
            allocation_bitmap: bitvec!(),
        }
    }
}

impl PageFrames {
    /// suggest that `page_count` pages will be allocated soon.
    /// useful to avoid frequent reallocations.
    fn reserve(&mut self, page_count: u32) {
        self.frames.reserve(page_count as usize);
        self.allocation_bitmap.reserve(page_count as usize);
    }

    /// allocate a new page frame, returning the PFN.
    /// page frame contents will be empty.
    fn allocate(&mut self) -> PFN {
        let maybe_free_index = self
            .allocation_bitmap
            .iter()
            .enumerate()
            .find(|(_, b)| !**b)
            .map(|(i, _)| i);

        if let Some(pfn) = maybe_free_index {
            self.allocation_bitmap.set(pfn, true);
            pfn as PFN
        } else {
            self.frames.push(EMPTY_PAGE);
            self.allocation_bitmap.push(true);
            (self.frames.len() - 1) as PFN
        }
    }

    /// deallocate a page by its PFN.
    /// panics if the page is not allocated.
    fn deallocate(&mut self, pfn: PFN) {
        assert!(self.allocation_bitmap.get(pfn as usize).unwrap());

        // zero pages upon deallocation.
        self.frames[pfn as usize] = EMPTY_PAGE;
        self.allocation_bitmap.set(pfn as usize, false);
    }
}

impl std::ops::Index<PFN> for PageFrames {
    type Output = PageFrame;

    fn index(&self, index: PFN) -> &PageFrame {
        &self.frames[index as usize]
    }
}

impl std::ops::IndexMut<PFN> for PageFrames {
    fn index_mut(&mut self, index: PFN) -> &mut PageFrame {
        &mut self.frames[index as usize]
    }
}

bitflags! {
    pub struct PageFlags: u32 {
        /// matches [crate::module::Permissions]
        const PERM_R = 0b00000001;
        const PERM_W = 0b00000010;
        const PERM_X = 0b00000100;
        const PERM_RW = Self::PERM_R.bits | Self::PERM_W.bits;
        const PERM_RX =  Self::PERM_R.bits | Self::PERM_X.bits;
        const PERM_WX =  Self::PERM_W.bits | Self::PERM_X.bits;
        const PERM_RWX =  Self::PERM_R.bits | Self::PERM_W.bits | Self::PERM_X.bits;

        /// a zero page, not backed by a Page Frame.
        /// upon write, allocate Page Frame on demand.
        const ZERO = 0b00001000;

        /// upon write, allocate Page Frame, copy frame, update mapping, and do the write.
        const COW = 0b00010000;
    }
}

#[derive(Error, Debug)]
pub enum MMUError {
    #[error("address already mapped: {0:#x}")]
    AddressAlreadyMapped(VA),
    #[error("address not mapped: {0:#x}")]
    AddressNotMapped(VA),
    #[error("address not readable: {0:#x}")]
    AddressNotReadable(VA),
    #[error("address not writable: {0:#x}")]
    AddressNotWritable(VA),
}

#[derive(Default, Clone)]
pub struct MMU {
    pages:   PageFrames,
    mapping: BTreeMap<VA, (PFN, PageFlags)>,
}

fn is_page_aligned(va: VA) -> bool {
    va & PAGE_MASK == 0x0
}

fn page_number(va: VA) -> u64 {
    (va >> PAGE_SHIFT) << PAGE_SHIFT
}

fn page_offset(va: VA) -> usize {
    (va & PAGE_MASK) as usize
}

impl MMU {
    // map memory at the given virtual address, for the given size, with the given
    // perms. panics if `addr` or `size` are not page aligned.
    pub fn mmap(&mut self, addr: VA, size: u64, perms: Permissions) -> Result<()> {
        assert!(is_page_aligned(addr));
        assert!(is_page_aligned(size));

        let page_count = size / PAGE_SIZE as u64;
        assert!(page_count <= u32::MAX as u64);

        // ensure none of the pages are already mapped.
        // Linux mmap updates any existing mappings.
        // I'm not sure if we'd prefer to go that route or not.
        // being conservative here will help us find bugs early on.
        //
        // do this all at once up front to avoid getting
        // half way through and needing to bail.
        for i in 0..page_count {
            let page_va = addr + i * PAGE_SIZE as u64;
            if self.mapping.contains_key(&page_va) {
                return Err(MMUError::AddressAlreadyMapped(page_va).into());
            }
        }

        self.pages.reserve(page_count as u32);

        let flags = PageFlags::ZERO | PageFlags::from_bits_truncate(perms.bits() as u32);
        for i in 0..page_count {
            let page_va = addr + i * PAGE_SIZE as u64;

            // initially, don't allocate any page frames, just use zero pages.
            // only when written to should we allocate page on demand.
            // this should be just as fast, since we've reserved the pages above.
            self.mapping.insert(page_va, (INVALID_PFN, flags));
        }

        Ok(())
    }

    pub fn munmap(&mut self, addr: VA, size: u64) -> Result<()> {
        assert!(is_page_aligned(addr));
        assert!(is_page_aligned(size));

        let page_count = size / PAGE_SIZE as u64;
        assert!(page_count <= u32::MAX as u64);

        // ensure all of the pages are already mapped.
        //
        // do this all at once up front to avoid getting
        // half way through and needing to bail.
        for i in 0..page_count {
            let page_va = addr + i * PAGE_SIZE as u64;
            if !self.mapping.contains_key(&page_va) {
                return Err(MMUError::AddressNotMapped(page_va).into());
            }
        }

        for i in 0..page_count {
            let page_va = addr + i * PAGE_SIZE as u64;

            let (pfn, flags) = self.mapping.remove(&page_va).unwrap();

            if !flags.intersects(PageFlags::ZERO) {
                self.pages.deallocate(pfn);
            } else {
                assert!(pfn == INVALID_PFN);
            }
        }

        Ok(())
    }

    fn probe_read(&self, addr: VA) -> Result<(PFN, PageFlags)> {
        let (pfn, flags) = match self.mapping.get(&page_number(addr)) {
            Some(&(pfn, flags)) => (pfn, flags),
            None => return Err(MMUError::AddressNotMapped(addr).into()),
        };

        if !flags.intersects(PageFlags::PERM_R) {
            return Err(MMUError::AddressNotReadable(addr).into());
        }

        Ok((pfn, flags))
    }

    /// read up to one page worth of data from the given address.
    /// read will not span more than two pages.
    fn read(&self, addr: VA, buf: &mut [u8]) -> Result<()> {
        assert!(buf.len() <= PAGE_SIZE);

        if page_number(addr) != page_number(addr + buf.len() as u64) {
            // split read
            let read_size: usize = buf.len();
            let page_offset = page_offset(addr);
            let first_size = PAGE_SIZE - page_offset;
            let second_size = read_size - first_size;

            let (first_pfn, first_flags) = self.probe_read(addr)?;

            if first_flags.intersects(PageFlags::ZERO) {
                assert!(first_pfn == INVALID_PFN);
                for b in &mut buf[..first_size] {
                    *b = 0;
                }
            } else {
                let first_part = &self.pages[first_pfn][page_offset..];
                buf[..first_part.len()].copy_from_slice(first_part);
            }

            let next_page_addr = addr + first_size as u64;
            let (second_pfn, second_flags) = self.probe_read(next_page_addr)?;

            if second_flags.intersects(PageFlags::ZERO) {
                assert!(second_pfn == INVALID_PFN);
                for b in &mut buf[first_size..] {
                    *b = 0;
                }
            } else {
                let second_part = &self.pages[second_pfn][..second_size];
                buf[first_size..].copy_from_slice(second_part);
            }
        } else {
            // common case: all data in single page
            let (pfn, flags) = self.probe_read(addr)?;

            if flags.intersects(PageFlags::ZERO) {
                // paranoia
                assert!(pfn == INVALID_PFN);

                for b in &mut buf[..] {
                    *b = 0;
                }

                return Ok(());
            }

            buf.copy_from_slice(&self.pages[pfn][page_offset(addr)..page_offset(addr) + buf.len()]);
        }
        Ok(())
    }

    pub fn read_u8(&self, addr: VA) -> Result<u8> {
        let mut buf = [0u8; std::mem::size_of::<u8>()];
        self.read(addr, &mut buf)?;
        Ok(buf[0])
    }

    pub fn read_u16(&self, addr: VA) -> Result<u16> {
        let mut buf = [0u8; std::mem::size_of::<u16>()];
        self.read(addr, &mut buf)?;
        Ok(LittleEndian::read_u16(&buf))
    }

    pub fn read_u32(&self, addr: VA) -> Result<u32> {
        let mut buf = [0u8; std::mem::size_of::<u32>()];
        self.read(addr, &mut buf)?;
        Ok(LittleEndian::read_u32(&buf))
    }

    pub fn read_u64(&self, addr: VA) -> Result<u64> {
        let mut buf = [0u8; std::mem::size_of::<u64>()];
        self.read(addr, &mut buf)?;
        Ok(LittleEndian::read_u64(&buf))
    }

    pub fn read_u128(&self, addr: VA) -> Result<u128> {
        let mut buf = [0u8; std::mem::size_of::<u128>()];
        self.read(addr, &mut buf)?;
        Ok(LittleEndian::read_u128(&buf))
    }

    // read one page worth of data from the given page-aligned address.
    // panics if `addr` is not page-aligned.
    pub fn read_page(&self, addr: VA) -> Result<[u8; PAGE_SIZE]> {
        assert!(is_page_aligned(addr));
        let mut buf = [0u8; PAGE_SIZE];
        self.read(addr, &mut buf)?;
        Ok(buf)
    }

    /// ensure that the given address can be written to, and if so,
    /// do any copies necessary due to COW/zero pages.
    fn probe_write(&mut self, addr: VA) -> Result<(PFN, PageFlags)> {
        let (pfn, flags) = match self.mapping.get(&page_number(addr)) {
            Some(&(pfn, flags)) => (pfn, flags),
            None => return Err(MMUError::AddressNotMapped(addr).into()),
        };

        if !flags.intersects(PageFlags::PERM_W) {
            return Err(MMUError::AddressNotWritable(addr).into());
        }

        if flags.intersects(PageFlags::ZERO) || flags.intersects(PageFlags::COW) {
            // collect a copy of the existing page frame contents
            let pf = if flags.intersects(PageFlags::ZERO) {
                EMPTY_PAGE
            } else {
                self.pages[pfn]
            };

            // and write it into a newly allocated page frame
            let pfn = self.pages.allocate();
            self.pages[pfn] = pf;

            // now update the mapping to point to the new pf
            let mut flags = flags;
            flags.remove(PageFlags::ZERO);
            flags.remove(PageFlags::COW);

            self.mapping.insert(page_number(addr), (pfn, flags));
            Ok((pfn, flags))
        } else {
            Ok((pfn, flags))
        }
    }

    /// write up one one page worth of data to the given address.
    fn write(&mut self, addr: VA, buf: &[u8]) -> Result<()> {
        assert!(buf.len() <= PAGE_SIZE);

        if page_number(addr) != page_number(addr + buf.len() as u64) {
            // split write
            let write_size: usize = buf.len();
            let page_offset = page_offset(addr);
            let first_size = PAGE_SIZE - page_offset;
            let second_size = write_size - first_size;

            let (first_pfn, _) = self.probe_write(addr)?;
            // if we fail sometime after this,
            // then we'll potentially have done a bit of extra work,
            // if the `probe_write` call did a COW/zero page copy.
            // but we assume thats not much/common overhead.
            // it also doesn't affect correctness, just slight performance hit.

            self.pages[first_pfn][page_offset..].copy_from_slice(&buf[..first_size]);

            let next_page_addr = addr + first_size as u64;
            let (second_pfn, _) = self.probe_write(next_page_addr)?;

            self.pages[second_pfn][..second_size].copy_from_slice(&buf[first_size..]);
        } else {
            // common case: all data in single page
            let (pfn, _) = self.probe_write(addr)?;

            self.pages[pfn][page_offset(addr)..page_offset(addr) + buf.len()].copy_from_slice(buf);
        }
        Ok(())
    }

    pub fn write_u8(&mut self, addr: VA, value: u8) -> Result<()> {
        let buf = [value];
        self.write(addr, &buf)
    }

    pub fn write_u16(&mut self, addr: VA, value: u16) -> Result<()> {
        let mut buf = [0u8; std::mem::size_of::<u16>()];
        LittleEndian::write_u16(&mut buf, value);
        self.write(addr, &buf)
    }

    pub fn write_u32(&mut self, addr: VA, value: u32) -> Result<()> {
        let mut buf = [0u8; std::mem::size_of::<u32>()];
        LittleEndian::write_u32(&mut buf, value);
        self.write(addr, &buf)
    }

    pub fn write_u64(&mut self, addr: VA, value: u64) -> Result<()> {
        let mut buf = [0u8; std::mem::size_of::<u64>()];
        LittleEndian::write_u64(&mut buf, value);
        self.write(addr, &buf)
    }

    pub fn write_u128(&mut self, addr: VA, value: u128) -> Result<()> {
        let mut buf = [0u8; std::mem::size_of::<u128>()];
        LittleEndian::write_u128(&mut buf, value);
        self.write(addr, &buf)
    }

    // write one page worth of data from the given page-aligned address.
    // panics if `addr` is not page-aligned.
    // panics if `value` is not one page in size.
    pub fn write_page(&mut self, addr: VA, value: &[u8]) -> Result<()> {
        assert!(is_page_aligned(addr));
        assert!(value.len() == PAGE_SIZE);
        self.write(addr, value)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(test)]
    mod pf {
        use crate::emu::mmu::*;
        use anyhow::Result;

        #[test]
        fn allocate() -> Result<()> {
            let mut pfs: PageFrames = Default::default();

            assert_eq!(pfs.allocate(), 0);
            assert_eq!(pfs.allocate(), 1);

            Ok(())
        }

        #[test]
        fn deallocate() -> Result<()> {
            let mut pfs: PageFrames = Default::default();

            assert_eq!(pfs.allocate(), 0);
            pfs.deallocate(0);
            assert_eq!(pfs.allocate(), 0);
            assert_eq!(pfs.allocate(), 1);
            pfs.deallocate(0);
            assert_eq!(pfs.allocate(), 0);

            Ok(())
        }

        #[test]
        fn reserve() -> Result<()> {
            let mut pfs: PageFrames = Default::default();
            pfs.reserve(1);

            // no change in behavior
            assert_eq!(pfs.allocate(), 0);
            pfs.deallocate(0);
            assert_eq!(pfs.allocate(), 0);

            Ok(())
        }

        #[test]
        fn index() -> Result<()> {
            let mut pfs: PageFrames = Default::default();

            assert_eq!(pfs.allocate(), 0);
            assert_eq!(pfs[0], EMPTY_PAGE);

            {
                let pf = &mut pfs[0];
                pf[0] = 0xFF;
            }

            assert_ne!(pfs[0], EMPTY_PAGE);

            Ok(())
        }
    }

    #[cfg(test)]
    mod mmu {
        use crate::emu::mmu::*;
        use anyhow::Result;

        #[test]
        fn access_violation() -> Result<()> {
            let mmu: MMU = Default::default();

            assert!(mmu.read_u8(0x0).is_err());
            assert!(mmu.read_u8(0x1).is_err());

            Ok(())
        }

        #[test]
        fn mmap() -> Result<()> {
            let mut mmu: MMU = Default::default();

            assert!(mmu.read_u8(0x1000).is_err());

            mmu.mmap(0x1000, 0x2000, Permissions::R).unwrap();

            assert!(mmu.read_u8(0xFFF).is_err());
            assert_eq!(mmu.read_u8(0x1000).unwrap(), 0x0);
            assert_eq!(mmu.read_u8(0x2000).unwrap(), 0x0);
            assert!(mmu.read_u8(0x3000).is_err());

            Ok(())
        }

        #[test]
        fn munmap() -> Result<()> {
            let mut mmu: MMU = Default::default();

            assert!(mmu.read_u8(0x1000).is_err());
            assert!(mmu.read_u8(0x2000).is_err());
            assert!(mmu.read_u8(0x3000).is_err());

            mmu.mmap(0x1000, 0x3000, Permissions::R).unwrap();

            assert_eq!(mmu.read_u8(0x1000).unwrap(), 0x0);
            assert_eq!(mmu.read_u8(0x2000).unwrap(), 0x0);
            assert_eq!(mmu.read_u8(0x3000).unwrap(), 0x0);

            mmu.munmap(0x2000, 0x1000).unwrap();

            assert_eq!(mmu.read_u8(0x1000).unwrap(), 0x0);
            assert!(mmu.read_u8(0x2000).is_err());
            assert_eq!(mmu.read_u8(0x3000).unwrap(), 0x0);

            Ok(())
        }

        #[test]
        fn write_u8() -> Result<()> {
            let mut mmu: MMU = Default::default();

            assert!(mmu.write_u8(0x1000, 1).is_err());

            mmu.mmap(0x1000, 0x1000, Permissions::R).unwrap();
            mmu.mmap(0x2000, 0x1000, Permissions::RW).unwrap();

            assert!(mmu.write_u8(0x1000, 1).is_err());
            assert_eq!(mmu.read_u8(0x1000).unwrap(), 0);

            assert!(mmu.write_u8(0x2000, 1).is_ok());
            assert_eq!(mmu.read_u8(0x2000).unwrap(), 1);

            Ok(())
        }

        #[test]
        fn read() -> Result<()> {
            let mut mmu: MMU = Default::default();

            mmu.mmap(0x1000, 0x1000, Permissions::RW).unwrap();
            assert_eq!(mmu.read_u8(0x1000).unwrap(), 0);
            assert!(mmu.write_u8(0x1000, 1).is_ok());
            assert_eq!(mmu.read_u8(0x1000).unwrap(), 1);
            assert_eq!(mmu.read_u16(0x1000).unwrap(), 1);
            assert_eq!(mmu.read_u32(0x1000).unwrap(), 1);
            assert_eq!(mmu.read_u64(0x1000).unwrap(), 1);
            assert_eq!(mmu.read_u128(0x1000).unwrap(), 1);

            Ok(())
        }

        #[test]
        fn write() -> Result<()> {
            let mut mmu: MMU = Default::default();

            mmu.mmap(0x1000, 0x1000, Permissions::RW).unwrap();
            assert_eq!(mmu.read_u8(0x1000).unwrap(), 0);

            assert!(mmu.write_u8(0x1000, 1).is_ok());
            assert_eq!(mmu.read_u8(0x1000).unwrap(), 1);

            assert!(mmu.write_u16(0x1000, 0x1122).is_ok());
            assert_eq!(mmu.read_u16(0x1000).unwrap(), 0x1122);

            assert!(mmu.write_u32(0x1000, 0x11223344).is_ok());
            assert_eq!(mmu.read_u32(0x1000).unwrap(), 0x11223344);

            assert!(mmu.write_u64(0x1000, 0x1122334455667788).is_ok());
            assert_eq!(mmu.read_u64(0x1000).unwrap(), 0x1122334455667788);

            assert!(mmu.write_u128(0x1000, 0x112233445566778899AABBCCDDEEFF).is_ok());
            assert_eq!(mmu.read_u128(0x1000).unwrap(), 0x112233445566778899AABBCCDDEEFF);

            Ok(())
        }

        #[test]
        fn split_read() -> Result<()> {
            let mut mmu: MMU = Default::default();

            mmu.mmap(0x1000, 0x2000, Permissions::RW).unwrap();
            assert!(mmu.write_u8(0x1FFF, 0x11).is_ok());
            assert!(mmu.write_u8(0x2000, 0x22).is_ok());
            assert!(mmu.write_u8(0x2001, 0x33).is_ok());
            assert!(mmu.write_u8(0x2002, 0x44).is_ok());
            assert!(mmu.write_u8(0x2003, 0x55).is_ok());
            assert!(mmu.write_u8(0x2004, 0x66).is_ok());
            assert!(mmu.write_u8(0x2005, 0x77).is_ok());
            assert!(mmu.write_u8(0x2006, 0x88).is_ok());
            assert!(mmu.write_u8(0x2007, 0x99).is_ok());
            assert_eq!(mmu.read_u8(0x1FFF).unwrap(), 0x11);
            assert_eq!(mmu.read_u16(0x1FFF).unwrap(), 0x2211);
            assert_eq!(mmu.read_u32(0x1FFF).unwrap(), 0x44332211);
            assert_eq!(mmu.read_u64(0x1FFF).unwrap(), 0x8877665544332211);

            Ok(())
        }

        #[test]
        fn split_write() -> Result<()> {
            // ensure that tests `read` and `write` pass before triaging this one.
            let mut mmu: MMU = Default::default();

            mmu.mmap(0x1000, 0x2000, Permissions::RW).unwrap();

            assert!(mmu.write_u8(0x1FFF, 1).is_ok());
            assert_eq!(mmu.read_u8(0x1FFF).unwrap(), 1);

            assert!(mmu.write_u16(0x1FFF, 0x1122).is_ok());
            assert_eq!(mmu.read_u16(0x1FFF).unwrap(), 0x1122);

            assert!(mmu.write_u32(0x1FFF, 0x11223344).is_ok());
            assert_eq!(mmu.read_u32(0x1FFF).unwrap(), 0x11223344);

            assert!(mmu.write_u64(0x1FFF, 0x1122334455667788).is_ok());
            assert_eq!(mmu.read_u64(0x1FFF).unwrap(), 0x1122334455667788);

            assert!(mmu.write_u128(0x1FFF, 0x112233445566778899AABBCCDDEEFF).is_ok());
            assert_eq!(mmu.read_u128(0x1FFF).unwrap(), 0x112233445566778899AABBCCDDEEFF);

            Ok(())
        }
    }
}
