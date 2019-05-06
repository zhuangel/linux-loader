// Copyright (c) 2019 Intel Corporation. All rights reserved.
// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

extern crate vm_memory;

use std::error::{self, Error as KernelLoaderError};
use std::ffi::CStr;
use std::fmt::{self, Display};
use std::io::{Read, Seek, SeekFrom};
use std::mem;

use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap, GuestUsize};

#[allow(dead_code)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(non_upper_case_globals)]
#[cfg_attr(feature = "cargo-clippy", allow(clippy))]
mod elf;
mod struct_util;

#[derive(Debug, PartialEq)]
pub enum Error {
    BigEndianElfOnLittle,
    CommandLineCopy,
    CommandLineOverflow,
    InvalidElfMagicNumber,
    InvalidProgramHeaderSize,
    InvalidProgramHeaderOffset,
    InvalidProgramHeaderAddress,
    InvalidEntryAddress,
    InvalidKernelStartAddress,
    InitrdImageSizeTooLarge,
    ReadElfHeader,
    ReadKernelImage,
    ReadProgramHeader,
    ReadInitrdImage,
    SeekKernelStart,
    SeekElfStart,
    SeekProgramHeader,
    SeekInitrdImage,
}
pub type Result<T> = std::result::Result<T, Error>;

impl error::Error for Error {
    fn description(&self) -> &str {
        match self {
            Error::BigEndianElfOnLittle => {
                "Trying to load big-endian binary on little-endian machine"
            }
            Error::CommandLineCopy => "Failed writing command line to guest memory",
            Error::CommandLineOverflow => "Command line overflowed guest memory",
            Error::InvalidElfMagicNumber => "Invalid Elf magic number",
            Error::InvalidProgramHeaderSize => "Invalid program header size",
            Error::InvalidProgramHeaderOffset => "Invalid program header offset",
            Error::InvalidProgramHeaderAddress => "Invalid Program Header Address",
            Error::InvalidEntryAddress => "Invalid entry address",
            Error::InvalidKernelStartAddress => "Invalid kernel start address",
            Error::InitrdImageSizeTooLarge => "Initrd image size too large",
            Error::ReadElfHeader => "Unable to read elf header",
            Error::ReadKernelImage => "Unable to read kernel image",
            Error::ReadProgramHeader => "Unable to read program header",
            Error::ReadInitrdImage => "Unable to read initrd image",
            Error::SeekKernelStart => "Unable to seek to kernel start",
            Error::SeekElfStart => "Unable to seek to elf start",
            Error::SeekProgramHeader => "Unable to seek to program header",
            Error::SeekInitrdImage => "Unable to seek initrd image",
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Kernel Loader Error: {}", Error::description(self))
    }
}

pub struct ElfLoader;

impl ElfLoader {
    /// Loads a kernel from a vmlinux elf image to a slice
    ///
    /// kernel is loaded into guest memory at offset phdr.p_paddr specified by elf image.
    ///
    /// # Arguments
    ///
    /// * `guest_mem` - The guest memory region the kernel is written to.
    /// * `kernel_start` - The offset into 'guest _mem' at which to load the kernel.
    /// * `kernel_image` - Input vmlinux image.
    /// * `lowest_kernel_start` - This is the start of the high memory, kernel should above it.
    ///
    /// # Returns
    /// * GuestAddress - GuestAddress where kernel is loaded.
    /// * usize - the length of kernel image. Return this in case of other part
    ///           like initrd will be loaded adjacent to the kernel part.
    pub fn load_kernel<F>(
        guest_mem: &GuestMemoryMmap,
        kernel_start: Option<GuestAddress>,
        kernel_image: &mut F,
        lowest_kernel_start: Option<GuestAddress>,
    ) -> Result<(GuestAddress, GuestUsize)>
    where
        F: Read + Seek,
    {
        let mut ehdr: elf::Elf64_Ehdr = Default::default();
        kernel_image
            .seek(SeekFrom::Start(0))
            .map_err(|_| Error::SeekElfStart)?;
        unsafe {
            // read_struct is safe when reading a POD struct.  It can be used and dropped without issue.
            struct_util::read_struct(kernel_image, &mut ehdr).map_err(|_| Error::ReadElfHeader)?;
        }

        // Sanity checks
        if ehdr.e_ident[elf::EI_MAG0 as usize] != elf::ELFMAG0 as u8
            || ehdr.e_ident[elf::EI_MAG1 as usize] != elf::ELFMAG1
            || ehdr.e_ident[elf::EI_MAG2 as usize] != elf::ELFMAG2
            || ehdr.e_ident[elf::EI_MAG3 as usize] != elf::ELFMAG3
        {
            return Err(Error::InvalidElfMagicNumber);
        }
        if ehdr.e_ident[elf::EI_DATA as usize] != elf::ELFDATA2LSB as u8 {
            return Err(Error::BigEndianElfOnLittle);
        }
        if ehdr.e_phentsize as usize != mem::size_of::<elf::Elf64_Phdr>() {
            return Err(Error::InvalidProgramHeaderSize);
        }
        if (ehdr.e_phoff as usize) < mem::size_of::<elf::Elf64_Ehdr>() {
            // If the program header is backwards, bail.
            return Err(Error::InvalidProgramHeaderOffset);
        }
        if lowest_kernel_start.is_some() {
            if (ehdr.e_entry as u64) < lowest_kernel_start.unwrap().raw_value() {
                return Err(Error::InvalidEntryAddress);
            }
        }
        // where the kernel will be start loaded.
        let kernel_loaded_addr = match kernel_start {
            Some(start) => GuestAddress(start.raw_value() + (ehdr.e_entry as u64)),
            None => GuestAddress(ehdr.e_entry as u64),
        };

        kernel_image
            .seek(SeekFrom::Start(ehdr.e_phoff))
            .map_err(|_| Error::SeekProgramHeader)?;
        let phdrs: Vec<elf::Elf64_Phdr> = unsafe {
            // Reading the structs is safe for a slice of POD structs.
            struct_util::read_struct_slice(kernel_image, ehdr.e_phnum as usize)
                .map_err(|_| Error::ReadProgramHeader)?
        };

        let mut kernel_end: GuestUsize = 0;

        // Read in each section pointed to by the program headers.
        for phdr in &phdrs {
            if phdr.p_type != elf::PT_LOAD || phdr.p_filesz == 0 {
                continue;
            }

            kernel_image
                .seek(SeekFrom::Start(phdr.p_offset))
                .map_err(|_| Error::SeekKernelStart)?;

            // vmm does not specify where the kernel should be loaded, just
            // load it to the physical address p_paddr for each segment.
            let mem_offset = match kernel_start {
                Some(start) => start
                    .checked_add(phdr.p_paddr as u64)
                    .ok_or(Error::InvalidProgramHeaderAddress)?,
                None => GuestAddress(phdr.p_paddr as u64),
            };

            guest_mem
                .read_exact_from(mem_offset, kernel_image, phdr.p_filesz as usize)
                .map_err(|_| Error::ReadKernelImage)?;

            kernel_end = mem_offset.raw_value() as GuestUsize + phdr.p_memsz as GuestUsize;
        }

        Ok((kernel_loaded_addr, kernel_end))
    }
}

/// Writes the command line string to the given memory slice.
///
/// # Arguments
///
/// * `guest_mem` - A u8 slice that will be partially overwritten by the command line.
/// * `guest_addr` - The address in `guest_mem` at which to load the command line.
/// * `cmdline` - The kernel command line.
pub fn load_cmdline(
    guest_mem: &GuestMemoryMmap,
    guest_addr: GuestAddress,
    cmdline: &CStr,
) -> Result<()> {
    let len = cmdline.to_bytes().len();
    if len == 0 {
        return Ok(());
    }

    let end = guest_addr
        .checked_add(len as u64 + 1)
        .ok_or(Error::CommandLineOverflow)?; // Extra for null termination.
    if end > guest_mem.end_addr() {
        return Err(Error::CommandLineOverflow)?;
    }

    guest_mem
        .write_slice(cmdline.to_bytes_with_nul(), guest_addr)
        .map_err(|_| Error::CommandLineCopy)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use vm_memory::{Address, GuestAddress, GuestMemoryMmap};
    use std::io::Cursor;

    const MEM_SIZE: u64 = 0x1000000;

    fn create_guest_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::new(&[(GuestAddress(0x0), (MEM_SIZE as usize))]).unwrap()
    }

    // Elf64 image that prints hello world on x86_64.
    fn make_elf_bin() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(include_bytes!("test_elf.bin"));
        v
    }

    #[test]
    fn load_elf() {
        let gm = create_guest_mem();
        let image = make_elf_bin();
        let kernel_addr = GuestAddress(0x200000);
        let mut lowest_kernel_start = GuestAddress(0x0);

        let mut x = ElfLoader::load_kernel(
            &gm,
            Some(kernel_addr),
            &mut Cursor::new(&image),
            Some(lowest_kernel_start),
        );
        assert_eq!(x.is_ok(), true);
        let mut entry_addr = x.unwrap().0;
        println!("load elf at address {:8x} \n", entry_addr.raw_value());

        x = ElfLoader::load_kernel(&gm, Some(kernel_addr), &mut Cursor::new(&image), None);
        assert_eq!(x.is_ok(), true);
        entry_addr = x.unwrap().0;
        println!("load elf at address {:8x} \n", entry_addr.raw_value());

        x = ElfLoader::load_kernel(
            &gm,
            None,
            &mut Cursor::new(&image),
            Some(lowest_kernel_start),
        );
        assert_eq!(x.is_ok(), true);
        entry_addr = x.unwrap().0;
        println!("load elf at address {:8x} \n", entry_addr.raw_value());

        lowest_kernel_start = GuestAddress(0xa00000);
        assert_eq!(
            Err(Error::InvalidEntryAddress),
            ElfLoader::load_kernel(&gm, None, &mut Cursor::new(&image), Some(lowest_kernel_start))
        );
    }

    #[test]
    fn cmdline_overflow() {
        let gm = create_guest_mem();
        let cmdline_address = GuestAddress(MEM_SIZE - 5);
        assert_eq!(
            Err(Error::CommandLineOverflow),
            load_cmdline(
                &gm,
                cmdline_address,
                CStr::from_bytes_with_nul(b"12345\0").unwrap()
            )
        );
    }

    #[test]
    fn cmdline_write_end() {
        let gm = create_guest_mem();
        let mut cmdline_address = GuestAddress(45);
        assert_eq!(
            Ok(()),
            load_cmdline(
                &gm,
                cmdline_address,
                CStr::from_bytes_with_nul(b"1234\0").unwrap()
            )
        );
        let val: u8 = gm.read_obj(cmdline_address).unwrap();
        assert_eq!(val, '1' as u8);
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj(cmdline_address).unwrap();
        assert_eq!(val, '2' as u8);
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj(cmdline_address).unwrap();
        assert_eq!(val, '3' as u8);
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj(cmdline_address).unwrap();
        assert_eq!(val, '4' as u8);
        cmdline_address = cmdline_address.unchecked_add(1);
        let val: u8 = gm.read_obj(cmdline_address).unwrap();
        assert_eq!(val, '\0' as u8);
    }

    #[test]
    fn bad_magic() {
        let gm = create_guest_mem();
        let kernel_addr = GuestAddress(0x0);
        let mut bad_image = make_elf_bin();
        bad_image[0x1] = 0x33;
        assert_eq!(
            Err(Error::InvalidElfMagicNumber),
            ElfLoader::load_kernel(&gm, Some(kernel_addr), &mut Cursor::new(&bad_image), None)
        );
    }

    #[test]
    fn bad_endian() {
        // Only little endian is supported
        let gm = create_guest_mem();
        let kernel_addr = GuestAddress(0x0);
        let mut bad_image = make_elf_bin();
        bad_image[0x5] = 2;
        assert_eq!(
            Err(Error::BigEndianElfOnLittle),
            ElfLoader::load_kernel(&gm, Some(kernel_addr), &mut Cursor::new(&bad_image), None)
        );
    }

    #[test]
    fn bad_phoff() {
        // program header has to be past the end of the elf header
        let gm = create_guest_mem();
        let kernel_addr = GuestAddress(0x0);
        let mut bad_image = make_elf_bin();
        bad_image[0x20] = 0x10;
        assert_eq!(
            Err(Error::InvalidProgramHeaderOffset),
            ElfLoader::load_kernel(&gm, Some(kernel_addr), &mut Cursor::new(&bad_image), None)
        );
    }
}