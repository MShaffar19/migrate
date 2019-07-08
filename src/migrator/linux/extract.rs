use failure::ResultExt;
use log::{debug, error, info, trace};
use nix::mount::umount;
use std::fs::{remove_dir, remove_file, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

// use serde::{Deserialize, Serialize};
use serde_yaml;

use crate::{
    common::{
        config::balena_config::ImageType,
        disk_util::{Disk, PartitionIterator, PartitionReader}, //  , ImageFile, GZipFile, PlainFile },
        Config,
        FileInfo,
        FileType,
        MigErrCtx,
        MigError,
        MigErrorKind,
    },
    defs::{PART_FSTYPE, PART_NAME},
    linux::{
        ensured_cmds::{EnsuredCmds, FILE_CMD, MKTEMP_CMD, MOUNT_CMD, TAR_CMD},
        linux_common::mktemp,
    },
};

// mod image_file;
// use image_file::ImageFile;

// mod gzip_file;
// use gzip_file::GZipFile;

// mod plain_file;
// use plain_file::PlainFile;

use crate::common::config::balena_config::{FSDump, PartDump};
use crate::common::path_append;

const REQUIRED_CMDS: &[&str] = &[FILE_CMD, MOUNT_CMD, MKTEMP_CMD, TAR_CMD];

const DEF_BLOCK_SIZE: usize = 512;
const DEF_BUFFER_SIZE: usize = 1024 * 1024;

const EXTRACT_FILE_TEMPLATE: &str = "extract.XXXXXXXXXX";
const MOUNTPOINT_TEMPLATE: &str = "mountpoint.XXXXXXXXXX";

const BUFFER_SIZE: usize = 1024 * 1024; // 1Mb

pub(crate) struct Partition {
    pub name: &'static str,
    pub fstype: &'static str,
    pub ptype: u8,
    pub status: u8,
    pub start_lba: u64,
    pub num_sectors: u64,
    pub archive: Option<PathBuf>,
}

pub(crate) struct Extractor {
    cmds: EnsuredCmds,
    config: Config,
    device_slug: String,
    disk: Disk,
}

// TODO: Extractor could modify config / save new ImageType
// TODO: Save ImageType as yml file

impl Extractor {
    pub fn new(config: Config) -> Result<Extractor, MigError> {
        trace!("new: entered");

        // TODO: support more devices
        let extract_device = if let Some(extract_device) = config.migrate.get_extract_device() {
            match extract_device {
                "beaglebone-black" => String::from(extract_device),
                "beaglebone-green" => String::from(extract_device),
                _ => {
                    error!("Unsupported device type for extract: {}", extract_device);
                    return Err(MigError::displayed());
                }
            }
        } else {
            error!("Missing the mandatory parameter extract-device",);
            return Err(MigError::displayed());
        };

        let mut cmds = EnsuredCmds::new();
        if let Err(why) = cmds.ensure_cmds(REQUIRED_CMDS) {
            error!(
                "Some Required commands could not be found, error: {:?}",
                why
            );
            return Err(MigError::displayed());
        }

        let image_file = if let ImageType::Flasher(image_file) = config.balena.get_image_path() {
            image_file
        } else {
            error!("The image path already points to an extracted configuration",);
            return Err(MigError::displayed());
        };

        let image_info = FileInfo::new(image_file, config.migrate.get_work_dir())?;

        if let Some(image_info) = image_info {
            debug!("new: working with file '{}'", image_info.path.display());
            if image_info.is_type(&cmds, &FileType::GZipOSImage)? {
                match Disk::from_gzip_img(&image_info.path) {
                    Ok(gzip_img) => {
                        debug!("new: is gzipped image '{}'", image_info.path.display());
                        return Ok(Extractor {
                            cmds,
                            config,
                            disk: gzip_img,
                            device_slug: String::from(extract_device),
                        });
                    }
                    Err(why) => {
                        error!(
                            "Unable to open the gzipped image file '{}', error: {:?}",
                            image_info.path.display(),
                            why
                        );
                        return Err(MigError::displayed());
                    }
                }
            } else {
                if image_info.is_type(&cmds, &FileType::OSImage)? {
                    match Disk::from_drive_file(&image_info.path, false, None) {
                        Ok(plain_img) => {
                            debug!("new: is plain image '{}'", image_info.path.display());
                            return Ok(Extractor {
                                cmds,
                                config,
                                disk: plain_img,
                                device_slug: extract_device,
                            });
                        }
                        Err(why) => {
                            error!(
                                "Unable to open the image file '{}', error: {:?}",
                                image_info.path.display(),
                                why
                            );
                            return Err(MigError::displayed());
                        }
                    }
                } else {
                    error!(
                        "Found an unexpected file type in '{}', not an OS image",
                        image_info.path.display()
                    );
                    return Err(MigError::displayed());
                }
            }
        } else {
            error!(
                "The image file could not be found: '{}'",
                image_file.display()
            );
            Err(MigError::displayed())
        }
    }

    pub fn extract(&mut self, output_path: Option<&Path>) -> Result<ImageType, MigError> {
        trace!("extract: entered");

        let mountpoint = match mktemp(
            &self.cmds,
            true,
            Some(MOUNTPOINT_TEMPLATE),
            Some(self.config.migrate.get_work_dir()),
        ) {
            Ok(path) => path,
            Err(why) => {
                error!(
                    "Failed to create temporary mountpoint for image extraction, error: {:?}",
                    why
                );
                return Err(MigError::displayed());
            }
        };

        // make file name
        let tmp_name = match mktemp(
            &self.cmds,
            false,
            Some(EXTRACT_FILE_TEMPLATE),
            Some(self.config.migrate.get_work_dir()),
        ) {
            Ok(path) => path,
            Err(why) => {
                error!(
                    "Failed to create temporary file for image extraction, error: {:?}",
                    why
                );
                return Err(MigError::displayed());
            }
        };

        let mut extract_err: Option<MigError> = None;
        // let mut part_extract_idx: usize = 0;

        let mut partitions: Vec<Partition> = Vec::new();

        let mut part_iterator = PartitionIterator::new(&mut self.disk)?;

        loop {
            let raw_part = if let Some(raw_part) = part_iterator.next() {
                raw_part
            } else {
                break;
            };

            let part_idx = partitions.len();
            let mut partition = Partition {
                name: PART_NAME[part_idx],
                fstype: PART_FSTYPE[part_idx],
                status: raw_part.status,
                ptype: raw_part.ptype,
                start_lba: raw_part.start_lba,
                num_sectors: raw_part.num_sectors,
                archive: None,
            };

            let mut part_reader =
                PartitionReader::from_part_iterator(&raw_part, &mut part_iterator);

            match Extractor::write_partition(
                &self.cmds,
                &self.config,
                &mut part_reader,
                &mut partition,
                &tmp_name,
                &mountpoint,
                output_path,
            ) {
                Ok(_) => {
                    info!(
                        "extracted partition: {}: to '{}'",
                        partition.name,
                        partition.archive.as_ref().unwrap().display()
                    );
                }
                Err(why) => {
                    error!(
                        "Failed to write partition {}: error: {:?}",
                        partition.name, why
                    );
                    extract_err = Some(why);
                    break;
                }
            }

            if let Some(_) = extract_err {
                break;
            }

            partitions.push(partition);
        }

        // TODO: try to umount
        let _res = remove_dir(&mountpoint);
        let _res = remove_file(&tmp_name);

        // late error exit after cleanup
        if let Some(why) = extract_err {
            return Err(why);
        }

        if partitions.len() == 5 {
            let res = ImageType::FileSystems(FSDump {
                device_slug: self.device_slug.clone(),
                check: None,
                boot: PartDump {
                    archive: partitions[0].archive.clone(),
                    blocks: partitions[0].num_sectors,
                },
                root_a: PartDump {
                    archive: partitions[1].archive.clone(),
                    blocks: partitions[1].num_sectors,
                },
                root_b: PartDump {
                    archive: partitions[2].archive.clone(),
                    blocks: partitions[2].num_sectors,
                },
                state: PartDump {
                    archive: partitions[3].archive.clone(),
                    blocks: partitions[3].num_sectors,
                },
                data: PartDump {
                    archive: partitions[4].archive.clone(),
                    blocks: partitions[4].num_sectors,
                },
            });

            debug!("res: {:?}", &res);

            let yaml_config = serde_yaml::to_string(&res).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!("Failed to serialize config to yaml"),
            ))?;

            let mut entabbed_cfg = String::new();
            let lines = yaml_config.lines();
            for line in lines {
                entabbed_cfg.push_str(&format!("    {}\n", line));
            }

            println!("image config:");
            println!("{}", entabbed_cfg);

            Ok(res)
        } else {
            error!(
                "Unexpected number of partitions found in image: '{}', {}",
                self.disk.get_image_file().display(),
                partitions.len()
            );
            Err(MigError::displayed())
        }
    }

    fn write_partition(
        cmds: &EnsuredCmds,
        config: &Config,
        part_reader: &mut PartitionReader,
        partition: &mut Partition,
        tmp_name: &Path,
        mountpoint: &Path,
        output_path: Option<&Path>,
    ) -> Result<(), MigError> {
        trace!(
            "write_partition: entered with tmp_name: '{}', mountpoint: '{}'",
            tmp_name.display(),
            mountpoint.display()
        );

        // TODO: cleanup on failure

        {
            // read partition contents to file
            let mut tmp_file = OpenOptions::new()
                .create(false)
                .write(true)
                .truncate(true)
                .open(&tmp_name)
                .context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!("Failed to opent temp file '{}'", tmp_name.display()),
                ))?;

            // TODO: check free disk space

            let mut buffer: [u8; DEF_BUFFER_SIZE] = [0; DEF_BUFFER_SIZE];
            loop {
                let bytes_read = part_reader
                    .read(&mut buffer)
                    .context(MigErrCtx::from_remark(
                        MigErrorKind::Upstream,
                        "Failed to read",
                    ))?;

                if bytes_read == 0 {
                    break;
                }

                let bytes_written =
                    tmp_file
                        .write(&buffer[0..bytes_read])
                        .context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!("Failed to write to '{}'", tmp_name.display()),
                        ))?;

                if bytes_read != bytes_written {
                    return Err(MigError::from_remark(
                        MigErrorKind::InvParam,
                        &format!(
                            "Read write bytes mismatch witing to '{}'",
                            tmp_name.display()
                        ),
                    ));
                }
            }

            debug!(
                "write_partition: partition written to '{}'",
                tmp_name.display()
            );
        }

        debug!(
            "write_partition: mounting '{}' on '{}'",
            tmp_name.display(),
            mountpoint.display()
        );

        // TODO: use losetup and then mount, mount -o loop seems to not work in ubuntu-14

        let cmd_res = cmds.call(
            MOUNT_CMD,
            &[
                "-t",
                &partition.fstype,
                "-o",
                "loop",
                &tmp_name.to_string_lossy(),
                &mountpoint.to_string_lossy(),
            ],
            true,
        )?;
        if !cmd_res.status.success() {
            return Err(MigError::from_remark(
                MigErrorKind::ExecProcess,
                &format!("Failed to mount extracted partition: {}", cmd_res.stderr),
            ));
        }

        // TODO: Try with builtin mount - not sure if loopmount is possible with this

        let arch_name = if let Some(output_path) = output_path {
            path_append(output_path, &format!("{}.tgz", partition.name))
        } else {
            path_append(
                config.migrate.get_work_dir(),
                &format!("{}.tgz", partition.name),
            )
        };

        // TODO: Try to archive using rust builtin tar / gzip have to traverse directories myself

        let cmd_res = cmds.call(
            TAR_CMD,
            &[
                "-czf",
                &arch_name.to_string_lossy(),
                "-C",
                &mountpoint.to_string_lossy(),
                ".",
            ],
            true,
        )?;

        if !cmd_res.status.success() {
            return Err(MigError::from_remark(
                MigErrorKind::ExecProcess,
                &format!(
                    "Failed to archive extracted partition, msg: {}",
                    cmd_res.stderr
                ),
            ));
        }

        debug!("write_partition: unmounting '{}'", mountpoint.display());
        umount(mountpoint).context(MigErrCtx::from_remark(
            MigErrorKind::Upstream,
            &format!("failed to unmount '{}'", mountpoint.display()),
        ))?;

        debug!(
            "write_partition: extracted partition '{}' to '{}'",
            partition.name,
            arch_name.display()
        );

        partition.archive = Some(arch_name.canonicalize().context(MigErrCtx::from_remark(
            MigErrorKind::Upstream,
            &format!("Failed to canonicalize path: '{}'", arch_name.display()),
        ))?);

        Ok(())
    }

    /*
        // Read partition table at offset up to the first empty or extended partition
        // return offset of next partition table for extended partition or None for end of table

        // TODO: ensure that about using 0 size partition as

        fn read_part_tbl(
            &mut self,
            offset: u64,
            table: &mut Vec<Partition>,
        ) -> Result<Option<u64>, MigError> {
            trace!("read_part_tbl: entered with offset {}", offset);
            let mut buffer: [u8; DEF_BLOCK_SIZE] = [0; DEF_BLOCK_SIZE];

            self.image_file
                .fill(offset * DEF_BLOCK_SIZE as u64, &mut buffer)?;

            let mbr: MasterBootRecord = unsafe { mem::transmute(buffer) };

            if (mbr.boot_sig1 != 0x55) || (mbr.boot_sig2 != 0xAA) {
                error!(
                    "invalid mbr sig1: {:x}, sig2: {:x}",
                    mbr.boot_sig1, mbr.boot_sig2
                );
                return Err(MigError::from_remark(
                    MigErrorKind::InvParam,
                    "unexpeted signatures found in partition table",
                ));
            }

            for partition in &mbr.part_tbl {
                let part_idx = table.len();

                if part_idx >= PART_NAME.len() || partition.num_sectors == 0 {
                    return Ok(None);
                }

                if (partition.ptype == 0xF) || (partition.ptype == 0x5) {
                    debug!(
                        "return extended partition offset: {}",
                        offset + partition.first_lba as u64
                    );
                    return Ok(Some(offset + partition.first_lba as u64));
                } else {
                    let part_info = Partition {
                        name: PART_NAME[part_idx],
                        fstype: PART_FSTYPE[part_idx],
                        start_lba: offset + partition.first_lba as u64,
                        num_sectors: partition.num_sectors as u64,
                        ptype: partition.ptype,
                        status: partition.status,
                        archive: None,
                    };

                    debug!(
                        "partition name: {}, fstype: {}, status: {:x}, type: {:x}, start: {}, size: {}",
                        part_info.name,
                        part_info.fstype,
                        part_info.status,
                        part_info.ptype,
                        part_info.start_lba,
                        part_info.num_sectors
                    );

                    table.push(part_info);
                }
            }
            debug!("return no further offset");
            Ok(None)
        }

    */
}
