// https://github.com/cs2dsb/stm32-usb.rs/blob/master/firmware/usb_bootloader/src/ghost_fat.rs

use core::ptr::read_volatile;
use std::marker::PhantomData;

#[cfg(feature = "defmt")]
use defmt::{debug, info, trace, warn, error};

#[cfg(not(feature = "defmt"))]
use log::{debug, info, trace, warn, error};

use packing::{Packed, PackedSize};

use usbd_scsi::{BlockDevice, BlockDeviceError};

pub mod config;
pub use config::Config;

pub mod boot;
use boot::FatBootBlock;

pub mod dir;
use dir::DirectoryEntry;

pub mod file;
use file::{File};

const UF2_SIZE: u32 = 0x10000 * 2;
const UF2_SECTORS: u32 = UF2_SIZE / (512 as u32);

const ASCII_SPACE: u8 = 0x20;


/// # Dummy fat implementation that provides a [UF2 bootloader](https://github.com/microsoft/uf2)
pub struct GhostFat<'a> {
    config: Config,
    fat_boot_block: FatBootBlock,
    pub(crate) fat_files: &'a mut [File<'a>],
}

impl <'a> GhostFat<'a> {
    pub fn new(files: &'a mut [File<'a>], config: Config) -> Self {
        Self {
            fat_boot_block: FatBootBlock::new(&config),
            fat_files: files,
            config,
        }
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BlockDeviceError> {
        let lba = addr / 512;
        let _offset = addr % 512;

      
        Ok(())
    }

}

impl <'a>BlockDevice for GhostFat<'a> {
    const BLOCK_BYTES: usize = 512;

    fn read_block(&self, lba: u32, block: &mut [u8]) -> Result<(), BlockDeviceError> {
        assert_eq!(block.len(), Self::BLOCK_BYTES);

        debug!("GhostFAT reading lba: {} ({} bytes)", lba, block.len());

        // Clear the buffer since we're sending all of it
        for b in block.iter_mut() {
            *b = 0
        }

        // Block 0 is the fat boot block
        if lba == 0 {
            self.fat_boot_block
                .pack(&mut block[..FatBootBlock::BYTES])
                .unwrap();
            block[510] = 0x55;
            block[511] = 0xAA;

        // File allocation table(s) follow the boot block
        } else if lba < self.config.start_rootdir() {
            let mut section_index = lba - self.config.start_fat0();

            // TODO: why?
            // https://github.com/lupyuen/bluepill-bootloader/blob/master/src/ghostfat.c#L207
            if section_index >= self.config.sectors_per_fat() {
                section_index -= self.config.sectors_per_fat();
            }

            // Track block indicies for each file
            let mut index = 1;

            // Set allocations for static files
            if section_index == 0 {
                block[0] = 0xF0;

                for f in self.fat_files.iter() {
                    // Determine number of blocks required for each file
                    let mut block_count = f.len() / Self::BLOCK_BYTES;
                    if f.len() % Self::BLOCK_BYTES != 0 {
                        block_count += 1;
                    }

                    // Write block allocations (2 byte)
                    for i in 0..block_count {
                        if i == block_count - 1 {
                            // Final block containes 0xFFFF
                            block[index + i] = 0xFF;
                            block[index + i + 1] = 0xFF;
                        } else {
                            // Preceding blocks should link to next object
                            // TODO: not sure this linking is correct... should split and test
                            block[index + i] = ((index + i + 2) >> 8) as u8;
                            block[index + i + 1] =  (index + i + 3) as u8;
                        }
                    }

                    // Increase block index
                    index += block_count * 2;
                }

                // Write trailer
                for i in 0..4 {
                    block[index + i] = 0xFF;
                }
                index += 4;

            }

            // Set remaining sectors as occupied
            for b in &mut block[index..] {
                *b = 0xFF;
            }

            // TODO: is this setting allocations for the uf2 file?
            // WTH is happening here and why is it load bearing..?

            // Assuming each file is one block, uf2 is offset by this
            let uf2_first_sector = self.fat_files.len() + 1;
            let uf2_last_sector = uf2_first_sector + UF2_SECTORS as usize - 1;

            for i in 0..256_usize {
                let v = section_index as usize * 256 + i;
                let j = 2 * i;
                if v >= uf2_first_sector && v < uf2_last_sector {
                    block[j + 0] = (((v + 1) >> 0) & 0xFF) as u8;
                    block[j + 1] = (((v + 1) >> 8) & 0xFF) as u8;
                } else if v == uf2_last_sector {
                    block[j + 0] = 0xFF;
                    block[j + 1] = 0xFF;
                }
            }


        // Directory entries follow
        } else if lba < self.config.start_clusters() {
            let section_index = lba - self.config.start_rootdir();
            if section_index == 0 {
                let mut dir = DirectoryEntry::default();
                dir.name.copy_from_slice(&self.fat_boot_block.volume_label);
                dir.attrs = 0x28;

                let len = DirectoryEntry::BYTES;
                dir.pack(&mut block[..len]).unwrap();
                dir.attrs = 0;

                // Starting cluster index (after BBL and FAT)
                let mut cluster_index = 2;

                // Generate directory entries for registered files
                for (i, info) in self.fat_files.iter().enumerate() {
                    // Determine number of blocks required for each file
                    let mut block_count = info.len() / Self::BLOCK_BYTES;
                    if info.len() % Self::BLOCK_BYTES != 0 {
                        block_count += 1;
                    }
                    dir.start_cluster = cluster_index as u16;
                    
                    // Write attributes
                    dir.name.copy_from_slice(&info.name_fat16_short().unwrap());
                    dir.size = info.len() as u32;
                    dir.attrs = info.attrs().bits();

                    // Encode to block
                    let start = (i + 1) * len;
                    dir.pack(&mut block[start..(start + len)]).unwrap();

                    // Increment cluster index
                    cluster_index += block_count;
                }
            }

        // Then finally clusters (containing actual data)
        } else {
            let section_index = (lba - self.config.start_clusters()) as usize;

            // Iterate through files to find matching block
            let mut block_index = 0;
            for f in self.fat_files.iter() {

                // Determine number of blocks required for each file
                let mut block_count = f.len() / Self::BLOCK_BYTES;
                if f.len() % Self::BLOCK_BYTES != 0 {
                    block_count += 1;
                }

                // If the LBA is within the file, return data
                if section_index < block_count + block_index {
                    let offset = section_index - block_index;

                    if let Some(chunk) = f.data().chunks(512).nth(offset) {
                        block[..chunk.len()].copy_from_slice(chunk);
                    }

                    return Ok(())
                }

                // Otherwise, continue
                block_index += block_count;
            }

            debug!("Unhandled read section: {}", section_index);
        }
        Ok(())
    }

    fn write_block(&mut self, lba: u32, block: &[u8]) -> Result<(), BlockDeviceError> {
        debug!("GhostFAT writing lba: {} ({} bytes)", lba, block.len());

        if lba == 0 {
            warn!("Attempted write to boot sector");
            return Ok(());

        // Write to FAT
        } else if lba < self.config.start_rootdir() {
            // TODO: should we support this?
            warn!("Attempted to write to FAT");

        // Write directory entry
        } else if lba < self.config.start_clusters() {
            // TODO: do we need to wrap this somehow to remap writes?
            warn!("Attempted to write directory entries");

            let section_index = lba - self.config.start_rootdir();
            if section_index == 0 {


            }

        // Write cluster data
        } else {
            let section_index = (lba - self.config.start_clusters()) as usize;

            // Iterate through files to find matching block
            let mut block_index = 0;
            for f in self.fat_files.iter_mut() {

                // Determine number of blocks required for each file
                let mut block_count = f.len() / Self::BLOCK_BYTES;
                if f.len() % Self::BLOCK_BYTES != 0 {
                    block_count += 1;
                }

                // If the LBA is within the file, write data
                if section_index < block_count + block_index {
                    let offset = section_index - block_index;

                    debug!("Write file: {} block: {}, {} bytes", f.name(), offset, block.len());

                    if let Some(chunk) = f.data_mut().map(|d| d.chunks_mut(512).nth(offset) ).flatten() {
                        let max_len = usize::min(block.len(), chunk.len());
                        chunk[..max_len].copy_from_slice(&block[..max_len])
                    } else {
                        error!("Attempted to write to read-only file");
                    }

                    return Ok(())
                }

                // Otherwise, continue
                block_index += block_count;
            }

            warn!("Unhandled write section: {}", section_index);
        }

        Ok(())
    }

    fn max_lba(&self) -> u32 {
        self.config.num_blocks - 1
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, Write, SeekFrom};
    use std::sync::{Arc, Mutex};
    use log::{trace, debug, info};

    use simplelog::{SimpleLogger, LevelFilter, Config as LogConfig};

    use fatfs::{FsOptions, FatType};
    use usbd_scsi::BlockDevice;

    use crate::{GhostFat, File, config::Config};

    pub struct MockDisk<'a> {
        pub index: usize,
        pub disk: GhostFat<'a>,
    }

    // TODO: read/write do not yet handle multiple blocks

    impl <'a> Read for MockDisk<'a> {
        fn read(&mut self, buff: &mut [u8]) -> std::io::Result<usize> {
            // Map block to index and buff len
            let mut lba = self.index as u32 / 512;
            let offset = self.index as usize % 512;

            let mut block = [0u8; 512];
            let mut index = 0;

            // If we're offset and reading > 1 block, handle partial block first
            if offset > 0 && buff.len() > (512 - offset) {
                trace!("Read offset chunk lba: {} offset: {} len: {}", lba, offset, 512-offset);

                // Read entire block
                self.disk.read_block(lba, &mut block).unwrap();

                // Copy offset portion
                buff[..512 - offset].copy_from_slice(&block[offset..]);

                // Update indexes
                index += 512 - offset;
                lba += 1;
            }

            // Then read remaining aligned blocks
            for c in (&mut buff[index..]).chunks_mut(512) {
                // Read whole block
                self.disk.read_block(lba, &mut block).unwrap();

                // Copy back requested chunk
                // Note offset can only be < BLOCK_SIZE when there's only one chunk
                c.copy_from_slice(&block[offset..][..c.len()]);

                // Update indexes
                index += c.len();
                lba += 1;
            }
            
            debug!("Read {} bytes at index 0x{:02x} (lba: {} offset: 0x{:02x}), data: {:02x?}", buff.len(), self.index, lba, offset, buff);

            // Increment index
            self.index += buff.len();

            Ok(buff.len())
        }
    }

    impl <'a> Write for MockDisk<'a> {
        fn write(&mut self, buff: &[u8]) -> std::io::Result<usize> {

            // Map block to index and buff len
            let lba = self.index as u32 / 512;
            let offset = self.index as usize % 512;

            debug!("Write {} bytes at index: 0x{:02x} (lba: {} offset: 0x{:02x}): data: {:02x?}", buff.len(), self.index, lba, offset, buff);


            {
                // Read whole block
                let mut block = [0u8; 512];
                self.disk.read_block(lba, &mut block).unwrap();

                // Apply write to block
                block[offset..][..buff.len()].copy_from_slice(buff);

                // Write whole block
                self.disk.write_block(lba, &block).unwrap();
            }

            #[cfg(nope)]
            // Direct write to provide more information in tests
            d.write(self.index as u32, buff).unwrap();

            // Increment index
            self.index += buff.len();

            Ok(buff.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            // No flush required as we're immediately writing back
            Ok(())
        }
    }

    impl <'a> Seek for MockDisk<'a> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            // Handle seek mechanisms
            match pos {
                SeekFrom::Start(v) => self.index = v as usize,
                SeekFrom::End(v) => {
                    todo!("Work out how long the disk is...");
                },
                SeekFrom::Current(v) => self.index = (self.index as i64 + v) as usize,
            }

            Ok(self.index as u64)
        }
    }

    fn setup<'a>(files: &'a mut [File<'a>]) -> MockDisk<'a> {
        let _ = simplelog::TermLogger::init(LevelFilter::Debug, LogConfig::default(), simplelog::TerminalMode::Mixed, simplelog::ColorChoice::Auto);

        let ghost_fat = GhostFat::new(files, Config::default());

        // Setup mock disk for fatfs
        let disk = MockDisk{
            index: 0,
            disk: ghost_fat,
        };

        disk
    }

    #[test]
    fn read_small_file() {

        // GhostFAT files
        let data = b"UF2 Bootloader 1.2.3\r\nModel: BluePill\r\nBoard-ID: xyz_123\r\n";
        let files = &mut [
            File::new("INFO_UF2.TXT", data).unwrap(),
        ];

        // Setup GhostFAT
        let disk = setup(files);

        // Setup fatfs
        let opts = FsOptions::new().update_accessed_date(false);
        let fs = fatfs::FileSystem::new(disk, opts).unwrap();
        assert_eq!(fs.fat_type(), FatType::Fat16);

        // Check base directory
        let root_dir = fs.root_dir();

        // Load files
        let f: Vec<_> = root_dir.iter().map(|v| v.unwrap() ).collect();
        log::info!("Files: {:?}", f);

        // Read first file
        assert_eq!(f[0].short_file_name(), "INFO_UF2.TXT");
        let mut f0 = f[0].to_file();
        
        let mut s0 = String::new();
        f0.read_to_string(&mut s0).unwrap();

        assert_eq!(s0.as_bytes(), data);
    }

    #[test]
    fn read_large_file() {

        let mut data = [0u8; 1024];
        for i in 0..data.len() {
            data[i] = rand::random::<u8>();
        }

        // GhostFAT files
        let files = &mut [
            File::new("TEST.BIN", &data).unwrap(),
        ];

        // Setup GhostFAT
        let disk = setup(files);

        // Setup fatfs
        let fs = fatfs::FileSystem::new(disk, FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), FatType::Fat16);

        // Check base directory
        let root_dir = fs.root_dir();

        // Load files
        let f: Vec<_> = root_dir.iter().map(|v| v.unwrap() ).collect();
        log::info!("Files: {:?}", f);

        // Read first file
        assert_eq!(f[0].short_file_name(), "TEST.BIN");
        let mut f0 = f[0].to_file();
        
        let mut v0 = Vec::new();
        f0.read_to_end(&mut v0).unwrap();

        assert_eq!(v0.as_slice(), data);
    }

    #[test]
    fn write_small_file() {

        // GhostFAT files
        let mut data = [0u8; 8];
        let files = &mut [
            File::new("TEST.TXT", data.as_mut()).unwrap(),
        ];

        // Setup GhostFAT
        let disk = setup(files);

        // Setup fatfs
        let fs = fatfs::FileSystem::new(disk, FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), FatType::Fat16);

        // Check base directory
        let root_dir = fs.root_dir();

        // Load files
        let f: Vec<_> = root_dir.iter().map(|v| v.unwrap() ).collect();
        log::info!("Files: {:?}", f);

        // Fetch first file
        assert_eq!(f[0].short_file_name(), "TEST.TXT");
        

        let d1 = b"DEF456\r\n";

        log::info!("Write file");

        // Rewind and write data
        let mut f0 = f[0].to_file();
        f0.write_all(d1).unwrap();
        f0.flush();
        drop(f0);

        log::info!("Read file");

        // Read back written data
        let mut f1 = f[0].to_file();
        let mut s0 = String::new();
        f1.read_to_string(&mut s0).unwrap();
        assert_eq!(s0.as_bytes(), d1);
    }

    #[test]
    fn write_large_file() {

        // GhostFAT files
        let mut data = [0u8; 1024];
        for i in 0..data.len() {
            data[i] = rand::random::<u8>();
        }

        let files = &mut [
            File::new("TEST.BIN", &mut data).unwrap(),
        ];

        // Setup GhostFAT
        let disk = setup(files);

        // Setup fatfs
        let fs = fatfs::FileSystem::new(disk, FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), FatType::Fat16);

        // Check base directory
        let root_dir = fs.root_dir();

        // Load files
        let f: Vec<_> = root_dir.iter().map(|v| v.unwrap() ).collect();
        log::info!("Files: {:?}", f);

        // Fetch first file
        assert_eq!(f[0].short_file_name(), "TEST.BIN");
        

        let mut d1 = [0u8; 1024];
        for i in 0..d1.len() {
            d1[i] = rand::random::<u8>();
        }

        // Rewind and write data
        let mut f0 = f[0].to_file();
        f0.rewind();
        f0.write_all(&d1).unwrap();
        f0.flush();
        drop(f0);

        // Read back written data
        let mut f1 = f[0].to_file();
        let mut v0 = Vec::new();
        f1.read_to_end(&mut v0).unwrap();
        assert_eq!(v0.as_slice(), d1);
    }

    #[test]
    fn read_many_files() {

        // GhostFAT files
        let d1 = b"abc123456";
        let d2 = b"abc123457";
        
        let files = &mut [
            File::new("TEST1.TXT", d1).unwrap(),
            File::new("TEST2.TXT", d2).unwrap(),
        ];

        // Setup GhostFAT
        let disk = setup(files);

        // Setup fatfs
        let fs = fatfs::FileSystem::new(disk, FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), FatType::Fat16);

        // Check base directory
        let root_dir = fs.root_dir();

        // Load files
        let f: Vec<_> = root_dir.iter().map(|v| v.unwrap() ).collect();
        log::info!("Files: {:?}", f);

        // Fetch first file
        assert_eq!(f[0].short_file_name(), "TEST1.TXT");
        
        // Read data
        let mut f1 = f[0].to_file();
        let mut s0 = String::new();
        f1.read_to_string(&mut s0).unwrap();
        assert_eq!(s0.as_bytes(), d1);

        // Fetch second file
        assert_eq!(f[1].short_file_name(), "TEST2.TXT");

        // Read data
        let mut f1 = f[1].to_file();
        let mut s0 = String::new();
        f1.read_to_string(&mut s0).unwrap();
        assert_eq!(s0.as_bytes(), d2);
    }
}
