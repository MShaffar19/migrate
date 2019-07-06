use failure::ResultExt;
use log::{debug, error, info, trace, warn, Level};
use mod_logger::{LogDestination, Logger, NO_STREAM};
use nix::{
    sys::reboot::{reboot, RebootMode},
    unistd::sync,
};

use std::fs::{copy, create_dir, read_dir};
// use std::io::{Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::{
    common::{
        dir_exists, file_exists, file_size, format_size_with_unit, path_append,
        stage2_config::{CheckedImageType, Stage2Config},
        MigErrCtx, MigError, MigErrorKind,
    },
    defs::{FailMode, BACKUP_FILE, SYSTEM_CONNECTIONS_DIR},
    linux::{
        device,
        ensured_cmds::{EnsuredCmds, REBOOT_CMD},
        linux_common::get_mem_info,
        linux_defs::{MIGRATE_LOG_FILE, STAGE2_MEM_THRESHOLD},
    },
};

// for starters just restore old boot config, only required command is mount

// later ensure all other required commands

mod fs_writer;

mod flasher;

pub(crate) mod mounts;
use core::borrow::BorrowMut;
use mounts::Mounts;
use std::cell::RefCell;

const REBOOT_DELAY: u64 = 3;
const S2_REV: u32 = 4;

// TODO: set this to Info once mature
const INIT_LOG_LEVEL: Level = Level::Trace;
const LOG_MOUNT_DIR: &str = "/migrate_log";
const LOG_FILE_NAME: &str = "migrate.log";

const MIGRATE_TEMP_DIR: &str = "/migrate_tmp";
const BOOT_MNT_DIR: &str = "mnt_boot";
const DATA_MNT_DIR: &str = "mnt_data";

const DD_BLOCK_SIZE: usize = 4194304;

const MIG_REQUIRED_CMDS: &[&str] = &[REBOOT_CMD];

const BALENA_IMAGE_FILE: &str = "balenaOS.img.gz";
const BALENA_CONFIG_FILE: &str = "config.json";

const BALENA_BOOT_FS_FILE: &str = "resin-boot.tgz";
const BALENA_ROOTA_FS_FILE: &str = "resin-rootA.tgz";
const BALENA_ROOTB_FS_FILE: &str = "resin-rootB.tgz";
const BALENA_STATE_FS_FILE: &str = "resin-state.tgz";
const BALENA_DATA_FS_FILE: &str = "resin-data.tgz";

pub(crate) enum FlashResult {
    Ok,
    FailRecoverable,
    FailNonRecoverable,
}

pub(crate) struct Stage2 {
    pub cmds: RefCell<EnsuredCmds>,
    pub mounts: RefCell<Mounts>,
    config: Stage2Config,
    pub recoverable_state: bool,
}

impl<'a> Stage2 {
    // try to mount former root device and /boot if it is on a separate partition and
    // load the stage2 config

    pub fn try_init() -> Result<Stage2, MigError> {
        Logger::set_default_level(&INIT_LOG_LEVEL);

        // log to stderr & memory buffer - so it can be saved to a persistent log later
        match Logger::set_log_dest(&LogDestination::BufferStderr, NO_STREAM) {
            Ok(_s) => {
                info!("Balena Migrate Stage 2 rev {} initializing", S2_REV);
            }
            Err(_why) => {
                println!("failed to initalize logger");
                println!("Balena Migrate Stage 2 rev {} initializing", S2_REV);
            }
        }

        let mut cmds = EnsuredCmds::new();
        if let Err(why) = cmds.ensure_cmds(MIG_REQUIRED_CMDS) {
            warn!("Not all required commands were found: {:?}", why);
        }

        // mount boot device containing BALENA_STAGE2_CFG for starters
        let mut mounts = match Mounts::new(&mut cmds) {
            Ok(mounts) => {
                debug!("Successfully mounted file system");
                mounts
            }
            Err(why) => {
                error!("Failed to mount boot file system, giving up: {:?}", why);
                return Err(MigError::displayed());
            }
        };

        debug!("got mounts: {:?}", mounts);

        let stage2_cfg_file = mounts.get_stage2_config();
        let stage2_cfg = match Stage2Config::from_config(&stage2_cfg_file) {
            Ok(s2_cfg) => {
                info!(
                    "Successfully read stage 2 config file from {}",
                    stage2_cfg_file.display()
                );
                s2_cfg
            }
            Err(why) => {
                error!(
                    "Failed to read stage 2 config file from file '{}' with error: {:?}",
                    stage2_cfg_file.display(),
                    why
                );
                // TODO: could try to restore former boot config anyway
                return Err(MigError::displayed());
            }
        };

        info!("Setting log level to {:?}", stage2_cfg.get_log_level());
        Logger::set_default_level(&stage2_cfg.get_log_level());

        // Mount all remaining drives - work and log
        match mounts.mount_from_config(&stage2_cfg) {
            Ok(_) => {
                info!("mounted all configured drives");
            }
            Err(why) => {
                warn!("mount_all returned an error: {:?}", why);
            }
        }

        // try switch logging to a persistent log
        let log_path = if let Some(log_path) = mounts.get_log_path() {
            Some(path_append(log_path, MIGRATE_LOG_FILE))
        } else {
            if stage2_cfg.is_no_flash() {
                if let Some(work_path) = mounts.get_work_path() {
                    Some(path_append(work_path, MIGRATE_LOG_FILE))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(log_path) = log_path {
            match Logger::set_log_file(&LogDestination::Stderr, &log_path, false) {
                Ok(_) => {
                    info!("Set log file to '{}'", log_path.display());
                    // Logger::flush();
                }
                Err(why) => {
                    warn!(
                        "Failed to set log file to '{}', error: {:?}",
                        log_path.display(),
                        why
                    );
                }
            }
        }

        return Ok(Stage2 {
            cmds: RefCell::new(cmds),
            mounts: RefCell::new(mounts),
            config: stage2_cfg,
            recoverable_state: false,
        });
    }

    // Do the actual work once drives are mounted and config is read

    pub fn migrate(&mut self) -> Result<(), MigError> {
        trace!("migrate: entered");

        let device_type = self.config.get_device_type();
        let boot_type = self.config.get_boot_type();

        // Recover device type and restore original boot configuration

        let device = device::from_config(device_type, boot_type)?;
        match device.restore_boot(&self.mounts.borrow(), &self.config) {
            Ok(_) => {
                info!("Boot configuration was restored sucessfully");
                // boot config restored can reboot
                self.recoverable_state = true;
            }
            Err(why) => {
                warn!(
                    "Failed to restore boot configuration - trying to migrate anyway. error: {:?}",
                    why
                );
            }
        }

        info!("migrating {:?} boot type: {:?}", device_type, boot_type);

        if let Err(why) = if let CheckedImageType::Flasher(ref _image_path) = self.config.get_balena_image().image {
            flasher::check_commands(&mut self.cmds.borrow_mut(), &self.config)
        } else {
            fs_writer::check_commands(&mut self.cmds.borrow_mut())
        } {
            error!("Some programs required to write the OS image to disk could not be located, error: '{:?}", why);
            return Err(MigError::displayed());
        }

        let work_path = if let Some(work_path) = self.mounts.borrow().get_work_path() {
            work_path.to_path_buf()
        } else {
            error!("The working directory was not mounted - aborting migration");
            return Err(MigError::displayed());
        };

        let mig_tmp_dir = if !self.mounts.borrow().is_work_no_copy() {
            // check if we have enough space to copy files to initramfs
            let mig_tmp_dir = match get_mem_info() {
                Ok((mem_tot, mem_avail)) => {
                    info!(
                        "Memory available is {} of {}",
                        format_size_with_unit(mem_avail),
                        format_size_with_unit(mem_tot)
                    );

                    let mut required_size = self.config.get_balena_image().req_space;

                    required_size +=
                        file_size(path_append(&work_path, &self.config.get_balena_config()))?;

                    if self.config.has_backup() {
                        required_size += file_size(path_append(&work_path, BACKUP_FILE))?;
                    }

                    let src_nwmgr_dir = path_append(&work_path, SYSTEM_CONNECTIONS_DIR);
                    if dir_exists(&src_nwmgr_dir)? {
                        let paths = read_dir(&src_nwmgr_dir).context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!("Failed to list directory '{}'", src_nwmgr_dir.display()),
                        ))?;

                        for path in paths {
                            if let Ok(path) = path {
                                required_size += file_size(path.path())?;
                            }
                        }
                    }

                    info!(
                        "Memory required for copying files is {}",
                        format_size_with_unit(required_size)
                    );

                    if mem_avail > required_size + STAGE2_MEM_THRESHOLD {
                        Path::new(MIGRATE_TEMP_DIR)
                    } else {
                        // TODO: create temporary storage on disk instead by resizing
                        error!("Not enough memory available for copying files");
                        return Err(MigError::from_remark(
                            MigErrorKind::InvState,
                            "Not enough memory available for copying files",
                        ));
                    }
                }
                Err(why) => {
                    warn!("Failed to retrieve mem info, error: {:?}", why);
                    return Err(MigError::displayed());
                }
            };

            if !dir_exists(mig_tmp_dir)? {
                create_dir(mig_tmp_dir).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "failed to create migrate temp directory {}",
                        MIGRATE_TEMP_DIR
                    ),
                ))?;
            }

            match self.config.get_balena_image().image {
                CheckedImageType::Flasher(ref image_file) => {
                    let src = path_append(&work_path, image_file);
                    let tgt = path_append(mig_tmp_dir, BALENA_IMAGE_FILE);
                    copy(&src, &tgt).context(MigErrCtx::from_remark(
                        MigErrorKind::Upstream,
                        &format!(
                            "failed to copy balena image to migrate temp directory, '{}' -> '{}'",
                            src.display(),
                            tgt.display()
                        ),
                    ))?;
                    info!("copied balena OS image to '{}'", tgt.display());
                }
                CheckedImageType::FileSystems(ref fs_dump) => {
                    if let Some(ref archive) = fs_dump.boot.archive {
                        let src = path_append(&work_path, archive);
                        let tgt = path_append(mig_tmp_dir, BALENA_BOOT_FS_FILE);
                        copy(&src, &tgt).context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!(
                                "failed to copy balena boot fs archive to migrate temp directory, '{}' -> '{}'",
                                src.display(),
                                tgt.display()
                            ),
                        ))?;
                        info!(
                            "copied balena boot archive to '{}' -> '{}'",
                            src.display(),
                            tgt.display()
                        );
                    } else {
                        error!(
                            "The balena boot archive was not configure - cannot partition drive"
                        );
                        return Err(MigError::displayed());
                    }

                    if let Some(ref archive) = fs_dump.root_a.archive {
                        let src = path_append(&work_path, archive);
                        let tgt = path_append(mig_tmp_dir, BALENA_ROOTA_FS_FILE);
                        copy(&src, &tgt).context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!(
                                "failed to copy balena root a fs archive to migrate temp directory, '{}' -> '{}'",
                                src.display(),
                                tgt.display()
                            ),
                        ))?;
                        info!(
                            "copied balena rootA archive to '{}' -> '{}'",
                            src.display(),
                            tgt.display()
                        );
                    } else {
                        error!(
                            "The balena root_a archive was not configure - cannot partition drive"
                        );
                        return Err(MigError::displayed());
                    }

                    if let Some(ref archive) = fs_dump.root_b.archive {
                        let src = path_append(&work_path, archive);
                        let tgt = path_append(mig_tmp_dir, BALENA_ROOTB_FS_FILE);
                        copy(&src, &tgt).context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!(
                                "failed to copy balena root b fs archive to migrate temp directory, '{}' -> '{}'",
                                src.display(),
                                tgt.display()
                            ),
                        ))?;
                        info!(
                            "copied balena rootB archive to '{}' -> '{}'",
                            src.display(),
                            tgt.display()
                        );
                    }

                    if let Some(ref archive) = fs_dump.state.archive {
                        let src = path_append(&work_path, archive);
                        let tgt = path_append(mig_tmp_dir, BALENA_STATE_FS_FILE);
                        copy(&src, &tgt).context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!(
                                "failed to copy balena state fs archive to migrate temp directory, '{}' -> '{}'",
                                src.display(),
                                tgt.display()
                            ),
                        ))?;
                        info!(
                            "copied balena state archive to '{}' -> '{}'",
                            src.display(),
                            tgt.display()
                        );
                    }

                    if let Some(ref archive) = fs_dump.data.archive {
                        let src = path_append(&work_path, archive);
                        let tgt = path_append(mig_tmp_dir, BALENA_DATA_FS_FILE);
                        copy(&src, &tgt).context(MigErrCtx::from_remark(
                            MigErrorKind::Upstream,
                            &format!(
                                "failed to copy balena data fs archive to migrate temp directory, '{}' -> '{}'",
                                src.display(),
                                tgt.display()
                            ),
                        ))?;
                        info!(
                            "copied balena data archive to '{}' -> '{}'",
                            src.display(),
                            tgt.display()
                        );
                    } else {
                        error!(
                            "The balena data archive was not configure - cannot partition drive"
                        );
                        return Err(MigError::displayed());
                    }
                }
            };

            let src = path_append(&work_path, self.config.get_balena_config());
            let tgt = path_append(mig_tmp_dir, BALENA_CONFIG_FILE);
            copy(&src, &tgt).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!(
                    "failed to copy balena config to migrate temp directory, '{}' -> '{}'",
                    src.display(),
                    tgt.display()
                ),
            ))?;

            info!("copied balena OS config to '{}'", tgt.display());

            let src_nwmgr_dir = path_append(&work_path, SYSTEM_CONNECTIONS_DIR);

            let tgt_nwmgr_dir = path_append(mig_tmp_dir, SYSTEM_CONNECTIONS_DIR);
            if dir_exists(&src_nwmgr_dir)? {
                if !dir_exists(&tgt_nwmgr_dir)? {
                    create_dir(&tgt_nwmgr_dir).context(MigErrCtx::from_remark(
                        MigErrorKind::Upstream,
                        &format!(
                            "failed to create systm-connections in migrate temp directory: '{}'",
                            tgt_nwmgr_dir.display()
                        ),
                    ))?;
                }

                let paths = read_dir(&src_nwmgr_dir).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!("Failed to list directory '{}'", src_nwmgr_dir.display()),
                ))?;

                for path in paths {
                    if let Ok(path) = path {
                        let src_path = path.path();
                        if src_path.metadata().unwrap().is_file() {
                            let tgt_path =
                                path_append(&tgt_nwmgr_dir, &src_path.file_name().unwrap());
                            copy(&src_path, &tgt_path)
                                .context(MigErrCtx::from_remark(MigErrorKind::Upstream, &format!("Failed copy network manager file to migrate temp directory '{}' -> '{}'", src_path.display(), tgt_path.display())))?;
                            info!("copied network manager config  to '{}'", tgt_path.display());
                        }
                    } else {
                        return Err(MigError::from_remark(
                            MigErrorKind::Upstream,
                            &format!(
                                "Error reading entry from directory '{}'",
                                src_nwmgr_dir.display()
                            ),
                        ));
                    }
                }
            }

            if self.config.has_backup() {
                // TODO: check available memory / disk space
                let target_path = path_append(mig_tmp_dir, BACKUP_FILE);
                let source_path = path_append(&work_path, BACKUP_FILE);

                copy(&source_path, &target_path).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "Failed copy backup file to migrate temp directory '{}' -> '{}'",
                        source_path.display(),
                        target_path.display()
                    ),
                ))?;
                info!("copied backup  to '{}'", target_path.display());
            }

            info!("Files copied to RAMFS");
            mig_tmp_dir
        } else {
            info!("Files were not copied, work dir is on a separate drive");
            // TODO: adapt path for no copy mode
            &work_path
        };

        // Write our buffered log to workdir before unmounting if we are not flashing anyway

        if self.config.is_no_flash() {
            // Logger::flush();
            let _res = Logger::set_log_dest(&LogDestination::StreamStderr, NO_STREAM);
        }

        let _res = self.mounts.borrow_mut().unmount_all();

        info!("Unmounted root file system");

        // ************************************************************************************
        // * write the gzipped image to disk
        // * from migrate:
        // * gzip -d -c "${MIGRATE_TMP}/${IMAGE_FILE}" | dd of=${BOOT_DEV} bs=4194304 || fail  "failed with gzip -d -c ${MIGRATE_TMP}/${IMAGE_FILE} | dd of=${BOOT_DEV} bs=4194304"

        let target_path = self.mounts.borrow().get_flash_device().to_path_buf();

        if !file_exists(&target_path) {
            return Err(MigError::from_remark(
                MigErrorKind::NotFound,
                &format!(
                    "Could not locate target device: '{}'",
                    target_path.display()
                ),
            ));
        }

        if self.config.is_no_flash() {
            info!("Not flashing due to config parameter no_flash");
            Stage2::exit(&FailMode::Reboot)?;
        }

        match self.config.get_balena_image().image {
            CheckedImageType::Flasher(ref image_file) => {
                // TODO: move some, if not most of this into flasher

                let image_path = if self.mounts.borrow().is_work_no_copy() {
                    if let Some(work_dir) = self.mounts.borrow().get_work_path() {
                        path_append(work_dir, image_file)
                    } else {
                        warn!("Work path not found in no_copy mode, trying mig temp");
                        path_append(mig_tmp_dir, BALENA_IMAGE_FILE)
                    }
                } else {
                    path_append(mig_tmp_dir, BALENA_IMAGE_FILE)
                };

                info!(
                    "attempting to flash '{}' to '{}'",
                    image_path.display(),
                    target_path.display()
                );

                if !file_exists(&image_path) {
                    return Err(MigError::from_remark(
                        MigErrorKind::NotFound,
                        &format!("Could not locate OS image: '{}'", image_path.display()),
                    ));
                }

                match flasher::flash_balena_os(
                    &target_path,
                    &self.cmds.borrow(),
                    &mut self.mounts.borrow_mut(),
                    &self.config,
                    &image_path,
                ) {
                    FlashResult::Ok => {}
                    FlashResult::FailRecoverable => {
                        error!("Failed to flash balena OS image");
                        // Logger::flush();
                        self.recoverable_state = true;
                        return Err(MigError::displayed());
                    }
                    FlashResult::FailNonRecoverable => {
                        error!("Failed to flash balena OS image");
                        // Logger::flush();
                        self.recoverable_state = false;
                        return Err(MigError::displayed());
                    }
                }

                // Logger::flush();
            }
            CheckedImageType::FileSystems(ref fs_dump) => {
                let base_path = if self.mounts.borrow().is_work_no_copy() {
                    if let Some(work_dir) = self.mounts.borrow().get_work_path() {
                        work_dir.to_path_buf()
                    } else {
                        warn!("Work path not found in no_copy mode, trying mig temp");
                        mig_tmp_dir.to_path_buf()
                    }
                } else {
                    mig_tmp_dir.to_path_buf()
                };

                match fs_writer::write_balena_os(
                    &target_path,
                    &self.cmds.borrow(),
                    &mut self.mounts.borrow_mut(),
                    &self.config,
                    &base_path,
                ) {
                    FlashResult::Ok => (),
                    FlashResult::FailNonRecoverable => {
                        self.recoverable_state = false;
                        error!("Failed to write balena os image");
                        return Err(MigError::displayed());
                    }
                    FlashResult::FailRecoverable => {
                        self.recoverable_state = true;
                        error!("Failed to write balena os image");
                        return Err(MigError::displayed());
                    }
                }
            }
        }

        // TODO: check fingerprints ?

        let boot_mountpoint =
            if let Some(mountpoint) = self.mounts.borrow().get_balena_boot_mountpoint() {
                mountpoint.to_path_buf()
            } else {
                self.recoverable_state = false;
                error!("Unable to retrieve balena boot mountpoint");
                return Err(MigError::displayed());
            };

        let src = path_append(mig_tmp_dir, BALENA_CONFIG_FILE);
        let tgt = path_append(&boot_mountpoint, BALENA_CONFIG_FILE);

        copy(&src, &tgt).context(MigErrCtx::from_remark(
            MigErrorKind::Upstream,
            &format!(
                "failed to copy balena config to boot mount dir, '{}' -> '{}'",
                src.display(),
                tgt.display()
            ),
        ))?;

        info!("copied balena OS config to '{}'", tgt.display());

        // copy system connections
        let nwmgr_dir = path_append(mig_tmp_dir, SYSTEM_CONNECTIONS_DIR);
        if dir_exists(&nwmgr_dir)? {
            let tgt_path = path_append(&boot_mountpoint, SYSTEM_CONNECTIONS_DIR);
            for path in read_dir(&nwmgr_dir).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!("Failed to read directory: '{}'", nwmgr_dir.display()),
            ))? {
                if let Ok(ref path) = path {
                    let tgt = path_append(&tgt_path, path.file_name());
                    copy(path.path(), &tgt).context(MigErrCtx::from_remark(
                        MigErrorKind::Upstream,
                        &format!(
                            "Failed to copy '{}' to '{}'",
                            path.path().display(),
                            tgt.display()
                        ),
                    ))?;
                    info!("copied '{}' to '{}'", path.path().display(), tgt.display());
                } else {
                    error!("failed to read path element: {:?}", path);
                }
            }
        } else {
            warn!("No network manager configurations were copied");
        }

        // we can hope to successfully reboot again after writing config.json and system-connections
        self.recoverable_state = true;

        if let Some(data_mountpoint) = self.mounts.borrow().get_balena_data_mountpoint() {
            // TODO: copy log, backup to data_path
            if self.config.has_backup() {
                // TODO: check available disk space
                let source_path = path_append(&mig_tmp_dir, BACKUP_FILE);
                let target_path = path_append(&data_mountpoint, BACKUP_FILE);

                copy(&source_path, &target_path).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "Failed copy backup file to data partition '{}' -> '{}'",
                        source_path.display(),
                        target_path.display()
                    ),
                ))?;
                info!("copied backup  to '{}'", target_path.display());
            }

            if Logger::get_log_dest().is_buffer_dest() {
                let log_path = path_append(&data_mountpoint, MIGRATE_LOG_FILE);
                match Logger::set_log_file(&LogDestination::Stderr, &log_path, false) {
                    Ok(_) => {
                        info!("Set log file to '{}'", log_path.display());
                        //Logger::flush();
                    }
                    Err(why) => {
                        warn!(
                            "Failed to set log file to '{}', error: {:?}",
                            log_path.display(),
                            why
                        );
                    }
                }
            }
        }

        let _res = self.mounts.borrow_mut().unmount_balena();

        info!(
            "Migration stage 2 was successful, rebooting in {} seconds!",
            REBOOT_DELAY
        );

        let _res = self.mounts.borrow_mut().unmount_log();

        thread::sleep(Duration::new(REBOOT_DELAY, 0));

        Stage2::exit(&FailMode::Reboot)?;

        Ok(())
    }

    fn exit(fail_mode: &FailMode) -> Result<(), MigError> {
        trace!("exit: entered with {:?}", fail_mode);

        Logger::flush();
        sync();

        match fail_mode {
            FailMode::Reboot => {
                reboot(RebootMode::RB_AUTOBOOT).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    "failed to reboot",
                ))?;
            }
            FailMode::RescueShell => {
                std::process::exit(1);
            }
        }
        Ok(())
    }

    /*
        pub(crate) fn is_recoverable(&self) -> bool {
            self.recoverable_state
        }
    */
    pub(crate) fn default_exit() -> Result<(), MigError> {
        trace!("default_exit: entered ");
        Stage2::exit(FailMode::get_default())
    }

    pub(crate) fn error_exit(&self) -> Result<(), MigError> {
        trace!("error_exit: entered");
        if self.recoverable_state {
            Stage2::exit(self.config.get_fail_mode())
        } else {
            Stage2::exit(&FailMode::RescueShell)
        }
    }
}

