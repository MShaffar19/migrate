use log::{debug, error, trace};
use std::io::{Error, ErrorKind, Read};
use std::mem;
use std::path::{Path, PathBuf};

use crate::common::{MigError, MigErrorKind};

mod image_file;
pub(crate) use image_file::ImageFile;

mod gzip_file;
pub(crate) use gzip_file::GZipFile;

mod plain_file;
pub(crate) use plain_file::PlainFile;

const DEF_BLOCK_SIZE: usize = 512;

// TODO: implement GPT partition

#[derive(Debug)]
pub(crate) enum PartitionType {
    Container,
    Fat,
    Linux,
    Empty,
    GPT,
    Other,
}

#[repr(C, packed)]
struct PartEntry {
    status: u8,
    first_head: u8,
    first_comb: u8,
    first_cyl: u8,
    ptype: u8,
    last_head: u8,
    last_comb: u8,
    last_cyl: u8,
    first_lba: u32,
    num_sectors: u32,
}

impl PartEntry {
    pub fn part_type(&self) -> PartitionType {
        // TODO: to be completed - currently only contains common, known partition types occurring in
        // encountered systems
        match self.ptype {
            0x00 => PartitionType::Empty,
            0x05 | 0x0f => PartitionType::Container,
            0xee => PartitionType::GPT,
            0x0c | 0x0e => PartitionType::Fat,
            0x83 => PartitionType::Linux,
            _ => PartitionType::Other,
        }
    }
}

#[repr(C, packed)]
struct MasterBootRecord {
    boot_code: [u8; 446],
    part_tbl: [PartEntry; 4],
    boot_sig1: u8,
    boot_sig2: u8,
}

#[derive(Debug, Clone)]
pub(crate) struct PartInfo {
    pub index: usize,
    pub ptype: u8,
    pub status: u8,
    pub start_lba: u64,
    pub num_sectors: u64,
}

#[derive(Debug)]
pub(crate) enum LabelType {
    GPT,
    Dos,
    Other,
}

impl LabelType {
    pub fn from_device<P: AsRef<Path>>(device_path: P) -> Result<LabelType, MigError> {
        let device_path = device_path.as_ref();
        // TODO: provide propper device block size
        Ok(Disk::from_drive_file(device_path, false, None)?.get_label()?)
    }
}

pub(crate) struct Disk {
    disk: Box<ImageFile>,
    writable: bool,
    block_size: u64,
}

impl Disk {
    pub fn from_gzip_img<P: AsRef<Path>>(image: P) -> Result<Disk, MigError> {
        Ok(Disk {
            disk: Box::new(GZipFile::new(image.as_ref())?),
            writable: false,
            block_size: DEF_BLOCK_SIZE as u64,
        })
    }

    pub fn from_drive_file<P: AsRef<Path>>(
        drive: P,
        writable: bool,
        block_size: Option<u64>,
    ) -> Result<Disk, MigError> {
        Ok(Disk {
            disk: Box::new(PlainFile::new(drive.as_ref())?),
            writable,
            block_size: if let Some(block_size) = block_size {
                block_size
            } else {
                DEF_BLOCK_SIZE as u64
            },
        })
    }

    pub fn get_image_file(&self) -> PathBuf {
        self.disk.get_path()
    }

    pub fn get_label(&mut self) -> Result<LabelType, MigError> {
        match self.read_mbr(0) {
            Ok(mbr) => match mbr.part_tbl[0].part_type() {
                PartitionType::GPT => Ok(LabelType::GPT),
                _ => Ok(LabelType::Dos),
            },
            Err(why) => {
                if why.kind() == MigErrorKind::InvParam {
                    Ok(LabelType::Other)
                } else {
                    Err(why)
                }
            }
        }
    }

    fn read_mbr(&mut self, block_idx: u64) -> Result<MasterBootRecord, MigError> {
        let mut buffer: [u8; DEF_BLOCK_SIZE] = [0; DEF_BLOCK_SIZE];

        self.disk
            .fill(block_idx * DEF_BLOCK_SIZE as u64, &mut buffer)?;

        let mbr: MasterBootRecord = unsafe { mem::transmute(buffer) };

        if (mbr.boot_sig1 != 0x55) || (mbr.boot_sig2 != 0xAA) {
            return Err(MigError::from_remark(
                MigErrorKind::InvParam,
                &format!(
                    "Encountered an invalid MBR signature, expected 0x55, 0xAA,  got {:x}, {:x}",
                    mbr.boot_sig1, mbr.boot_sig2
                ),
            ));
        }

        Ok(mbr)
    }

    /*
        pub fn get_partition_iterator(&mut self) -> Result<PartitionIterator, MigError> {
            Ok(PartitionIterator::new(self)?)
        }

        // TODO: allow reading partitions while holding PartitionIterator
        // PartitionIterator must supply partitionReader as it holds a mut ref to Disk

        pub fn get_partition_reader(
            &mut self,
            partition: &PartInfo,
        ) -> Result<PartitionReader, MigError> {
            Ok(PartitionReader::from_disk(partition, self)?)
        }
    */
}

pub(crate) struct PartitionIterator<'a> {
    disk: &'a mut Disk,
    mbr: Option<MasterBootRecord>,
    offset: u64,
    index: usize,
    part_idx: usize,
}

impl<'a> PartitionIterator<'a> {
    pub fn new(disk: &mut Disk) -> Result<PartitionIterator, MigError> {
        let offset: u64 = 0;
        let mbr = disk.read_mbr(offset)?;

        Ok(PartitionIterator {
            disk,
            mbr: Some(mbr),
            offset,
            index: 0,
            part_idx: 0,
        })
    }
}

// TODO: make functions for partition type:
// is extended
// is None
// is regular

impl<'a> Iterator for PartitionIterator<'a> {
    type Item = PartInfo;

    fn next(&mut self) -> Option<Self::Item> {
        trace!("PartitionIterator::next: entered");
        // TODO: check for 0 size partition ?

        enum SetMbr {
            Leave,
            ToNone,
            ToMbr(MasterBootRecord),
        }

        let (res, mbr) = if let Some(ref mbr) = self.mbr {
            if self.offset == 0 {
                debug!(
                    "PartitionIterator::next: offset: {}, index: {}, part_idx: {}, mbr: present",
                    self.offset, self.index, self.part_idx
                );
                // we are on the first partition table
                if self.index > 3 {
                    // end of regular partition table reached
                    (None, SetMbr::Leave)
                } else {
                    // read regular partition
                    let part = &mbr.part_tbl[self.index];
                    match part.part_type() {
                        PartitionType::Empty =>
                        // empty partition - Assume End of Table
                        {
                            (None, SetMbr::Leave)
                        }
                        PartitionType::Container => {
                            // extended / container
                            // return extended partition
                            self.offset = part.first_lba as u64;
                            // self.mbr = None; // we are done with this mbr
                            self.part_idx += 1;
                            (
                                Some(PartInfo {
                                    index: self.part_idx,
                                    ptype: part.ptype,
                                    status: part.status,
                                    start_lba: part.first_lba as u64,
                                    num_sectors: part.num_sectors as u64,
                                }),
                                SetMbr::ToNone,
                            )
                        }
                        PartitionType::Fat | PartitionType::Linux => {
                            // return regular partition
                            self.index += 1;
                            self.part_idx += 1;
                            (
                                Some(PartInfo {
                                    index: self.part_idx,
                                    ptype: part.ptype,
                                    status: part.status,
                                    start_lba: part.first_lba as u64,
                                    num_sectors: part.num_sectors as u64,
                                }),
                                SetMbr::Leave,
                            )
                        }
                        _ => {
                            error!("Unsupported partition type encountered: {:x}", part.ptype);
                            (None, SetMbr::Leave)
                        }
                    }
                }
            } else {
                // we are on an extended partitions table
                if self.index != 1 {
                    error!("Unexpected index into extended partition {}", self.index);
                    (None, SetMbr::Leave)
                } else {
                    // Extended partition tables should have only 2 entries. The actual partition
                    // which has already been reported (see None = self.mbr and below) and a pointer
                    // to the next extended partition which we would be looking at here

                    let part = &mbr.part_tbl[self.index];
                    match part.part_type() {
                        PartitionType::Empty => {
                            // regular end  of extended partitions
                            // // warn!("Empty partition on index 1 of extended partition is unexpected");
                            (None, SetMbr::Leave)
                        } // weird though
                        PartitionType::Container => {
                            // we are expecting a container partition here
                            self.offset += part.first_lba as u64;
                            match self.disk.read_mbr(self.offset) {
                                Ok(mbr) => {
                                    let part = &mbr.part_tbl[0];
                                    // self.mbr = Some(mbr)
                                    match part.part_type() {
                                        PartitionType::Linux | PartitionType::Fat => {
                                            self.index = 1;
                                            self.part_idx += 1;
                                            (
                                                Some(PartInfo {
                                                    index: self.part_idx,
                                                    ptype: part.ptype,
                                                    status: part.status,
                                                    start_lba: self.offset + part.first_lba as u64,
                                                    num_sectors: part.num_sectors as u64,
                                                }),
                                                SetMbr::ToMbr(mbr),
                                            )
                                        }
                                        _ => {
                                            error!("Unexpected partition type {:x} on index 0 of extended partition", part.ptype);
                                            (None, SetMbr::Leave)
                                        }
                                    }
                                }
                                Err(why) => {
                                    error!("Failed to read mbr, error:{:?}", why);
                                    (None, SetMbr::Leave)
                                }
                            }
                        }
                        _ => {
                            error!(
                                "Unexpected partition type {:x} on index 1 of extended partition",
                                part.ptype
                            );
                            (None, SetMbr::Leave)
                        }
                    }
                }
            }
        } else {
            // this only happens after the first extended partition has been reported
            debug!(
                "PartitionIterator::next: offset: {}, index: {}, part_idx: {}, mbr: absent",
                self.offset, self.index, self.part_idx
            );
            match self.disk.read_mbr(self.offset) {
                Ok(mbr) => {
                    debug!("PartitionIterator::next: got mbr");
                    let part = &mbr.part_tbl[0];
                    // self.mbr = Some(mbr);
                    let part_type = part.part_type();
                    debug!(
                        "PartitionIterator::next: got partition type: {:?}",
                        part_type
                    );
                    match part_type {
                        PartitionType::Empty => {
                            debug!("PartitionIterator::next: got empty partition");
                            // looks like the extended partition is empty
                            (None, SetMbr::ToMbr(mbr))
                        }
                        PartitionType::Fat | PartitionType::Linux => {
                            debug!("PartitionIterator::next: got partition data partition");
                            self.index = 1;
                            self.part_idx += 1;
                            (
                                Some(PartInfo {
                                    index: self.part_idx,
                                    ptype: part.ptype,
                                    status: part.status,
                                    start_lba: self.offset + part.first_lba as u64,
                                    num_sectors: part.num_sectors as u64,
                                }),
                                SetMbr::ToMbr(mbr),
                            )
                        }
                        _ => {
                            error!(
                                "Unexpected partition type {:x} on index 0 of extended partition",
                                part.ptype
                            );
                            (None, SetMbr::Leave)
                        }
                    }
                }
                Err(why) => {
                    error!("Failed to read mbr, error:{:?}", why);
                    (None, SetMbr::Leave)
                }
            }
        };

        debug!(
            "PartitionIterator::next Res: {}",
            if let Some(_) = res { "some" } else { "none" }
        );

        match mbr {
            SetMbr::ToMbr(mbr) => {
                debug!("PartitionIterator::next set mbr");
                self.mbr = Some(mbr);
            }
            SetMbr::Leave => {
                debug!("PartitionIterator::next leave mbr");
            }
            SetMbr::ToNone => {
                debug!("PartitionIterator::next reset mbr");
                self.mbr = None;
            }
        }

        res
    }
}

pub(crate) struct PartitionReader<'a> {
    disk: &'a mut Disk,
    offset: u64,
    bytes_left: u64,
}

impl<'a> PartitionReader<'a> {
    pub fn from_disk(part: &PartInfo, disk: &'a mut Disk) -> PartitionReader<'a> {
        let block_size = disk.block_size;
        PartitionReader {
            disk,
            offset: part.start_lba * block_size,
            bytes_left: part.num_sectors * block_size,
        }
    }

    pub fn from_part_iterator(
        part: &PartInfo,
        iterator: &'a mut PartitionIterator,
    ) -> PartitionReader<'a> {
        let block_size = iterator.disk.block_size;
        PartitionReader {
            disk: iterator.disk,
            offset: part.start_lba * block_size,
            bytes_left: part.num_sectors * block_size,
        }
    }
}

impl<'a> Read for PartitionReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if self.bytes_left == 0 {
            return Ok(0);
        } else {
            let (res, size) = if self.bytes_left < buf.len() as u64 {
                (
                    self.disk
                        .disk
                        .fill(self.offset, &mut buf[0..self.bytes_left as usize]),
                    self.bytes_left as usize,
                )
            } else {
                (self.disk.disk.fill(self.offset, buf), buf.len())
            };

            match res {
                Ok(_) => {
                    self.offset += size as u64;
                    self.bytes_left -= size as u64;
                    Ok(size)
                }
                Err(why) => Err(Error::new(ErrorKind::UnexpectedEof, why.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod test {

    use mod_logger::{Level, Logger};

    use crate::common::{
        disk_util::{Disk, LabelType},
        MigError,
    };

    #[test]
    fn read_gzipped_part() {
        Logger::set_default_level(&Level::Debug);
        let mut disk = Disk::from_gzip_img("./test_data/part.img.gz").unwrap();
        if let LabelType::Dos = disk.get_label().unwrap() {
            let mut count = 0;
            for partition in disk.get_partition_iterator().unwrap() {
                match partition.index {
                    1 => assert_eq!(partition.ptype, 0x0e),
                    4 => assert_eq!(partition.ptype, 0x05),
                    _ => assert_eq!(partition.ptype, 0x83),
                }
                count += 1;
            }
            assert_eq!(count, 6);
        } else {
            panic!("Invalid label type - not Dos");
        }
    }
}
