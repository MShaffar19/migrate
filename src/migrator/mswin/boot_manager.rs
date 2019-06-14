use failure::ResultExt;
use log::{debug, info};
use regex::Regex;
use std::fs::{copy, create_dir_all, rename, File};
use std::io::Write;

const STARTUP_TEMPLATE: &str = r#"
echo -off
echo Starting balena Migrate Environment
"#;

use crate::{
    common::{
        dir_exists, file_exists, path_append,
        stage2_config::{
            Stage2ConfigBuilder,
            BootMgrConfig
        },
        Config,
        MigErrCtx, MigError, MigErrorKind,
    },
    defs::{
        BootType, BALENA_EFI_DIR, EFI_BCKUP_DIR, EFI_BOOT_DIR, EFI_DEFAULT_BOOTMGR64,
        EFI_STARTUP_FILE, MIG_INITRD_NAME, MIG_KERNEL_NAME,
    },
    mswin::{migrate_info::MigrateInfo, msw_defs::EFI_MS_BOOTMGR},
};

pub(crate) trait BootManager {
    fn get_boot_type(&self) -> BootType;
    fn can_migrate(
        &mut self,
        mig_info: &MigrateInfo,
        config: &Config,
        s2_cfg: &mut Stage2ConfigBuilder,
    ) -> Result<bool, MigError>;
    fn setup(
        &self,
        mig_info: &MigrateInfo,
        config: &Config,
        s2_cfg: &mut Stage2ConfigBuilder,
    ) -> Result<(), MigError>;
}

pub(crate) struct EfiBootManager {
    msw_device: bool,
}

impl EfiBootManager {
    pub fn new() -> EfiBootManager {
        EfiBootManager { msw_device: true }
    }
}

impl BootManager for EfiBootManager {
    fn get_boot_type(&self) -> BootType {
        BootType::MSWEfi
    }

    fn can_migrate(
        &mut self,
        _dev_info: &MigrateInfo,
        _config: &Config,
        _s2_cfg: &mut Stage2ConfigBuilder,
    ) -> Result<bool, MigError> {
        Ok(true)
        // Err(MigError::from(MigErrorKind::NotImpl))
    }

    fn setup(
        &self,
        mig_info: &MigrateInfo,
        _config: &Config,
        s2_cfg: &mut Stage2ConfigBuilder,
    ) -> Result<(), MigError> {
        // for now:
        // copy our kernel & initramfs to \EFI\balena-migrate
        // move all boot manager files in
        //    \EFI\Boot\bootx86.efi
        //    \EFI\Microsoft\Boot\bootmgrfw.efi
        // to a safe place and add a
        // create a startup.nsh file in \EFI\Boot\ that refers to our kernel & initramfs

        if let Some(ref efi_path) = mig_info.drive_info.efi_path {

            s2_cfg.set_bootmgr_cfg(BootMgrConfig::new(
                &Pathbuf::from(efi_path.get_linux_part()),
                &Pathbuf::from(efi_path.get_linux_fstype()),
                &PathBuf::from(efi_path.get_path())));

            let balena_efi_dir = path_append(efi_path.get_path(), BALENA_EFI_DIR);
            if !dir_exists(&balena_efi_dir)? {
                create_dir_all(&balena_efi_dir).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "failed to create EFI directory '{}'",
                        balena_efi_dir.display()
                    ),
                ))?;
            }

            let kernel_path = path_append(&balena_efi_dir, MIG_KERNEL_NAME);
            debug!(
                "copy '{}' to '{}'",
                &mig_info.kernel_file.path.display(),
                &kernel_path.display()
            );
            copy(&mig_info.kernel_file.path, &kernel_path).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!(
                    "failed to copy migrate kernel to EFI directory '{}'",
                    kernel_path.display()
                ),
            ))?;
            let initrd_path = path_append(&balena_efi_dir, MIG_INITRD_NAME);
            debug!(
                "copy '{}' to '{}'",
                &mig_info.initrd_file.path.display(),
                &initrd_path.display()
            );
            copy(&mig_info.initrd_file.path, &initrd_path).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!(
                    "failed to copy migrate initramfs to EFI directory '{}'",
                    initrd_path.display()
                ),
            ))?;

            let efi_boot_dir = path_append(efi_path.get_path(), EFI_BOOT_DIR);
            if !dir_exists(&efi_boot_dir)? {
                create_dir_all(&balena_efi_dir).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "failed to create EFI directory '{}'",
                        balena_efi_dir.display()
                    ),
                ))?;
            }

            let startup_path = path_append(efi_boot_dir, EFI_STARTUP_FILE);

            debug!("writing '{}'", &startup_path.display());
            let drive_letter_re = Regex::new(r#"^[a-z,A-Z]:(.*)$"#).unwrap();
            let tmp_path = kernel_path.to_string_lossy();
            let kernel_path = if let Some(captures) = drive_letter_re.captures(&tmp_path) {
                captures.get(1).unwrap().as_str()
            } else {
                &tmp_path
            };
            let tmp_path = initrd_path.to_string_lossy();
            let initrd_path = if let Some(captures) = drive_letter_re.captures(&tmp_path) {
                captures.get(1).unwrap().as_str()
            } else {
                &tmp_path
            };

            // TODO: prefer PARTUUID to guessed device name

            let startup_content =
                if let Some(partuuid) = mig_info.drive_info.work_path.get_partuuid() {
                    format!(
                        "{}{} initrd={} root=PARTUUID={} rootfstype={}",
                        STARTUP_TEMPLATE,
                        kernel_path,
                        initrd_path,
                        partuuid,
                        mig_info.drive_info.work_path.get_linux_fstype()
                    )
                } else {
                    format!(
                        "{}{} initrd={} root={} rootfstype={}",
                        STARTUP_TEMPLATE,
                        kernel_path,
                        initrd_path,
                        mig_info
                            .drive_info
                            .work_path
                            .get_linux_part()
                            .to_string_lossy(),
                        mig_info.drive_info.work_path.get_linux_fstype()
                    )
                };

            let mut startup_file = File::create(&startup_path).context(MigErrCtx::from_remark(
                MigErrorKind::Upstream,
                &format!(
                    "failed to create EFI startup file '{}'",
                    startup_path.display()
                ),
            ))?;
            startup_file
                .write(startup_content.as_bytes())
                .context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "Failed to write EFI startup file'{}'",
                        startup_path.display()
                    ),
                ))?;

            let efi_bckup_dir = path_append(efi_path.get_path(), EFI_BCKUP_DIR);
            if !dir_exists(&efi_bckup_dir)? {
                create_dir_all(&efi_bckup_dir).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "failed to create EFI backup directory '{}'",
                        efi_bckup_dir.display()
                    ),
                ))?;
            }

            let msw_boot_mgr = path_append(efi_path.get_path(), EFI_MS_BOOTMGR);
            if file_exists(&msw_boot_mgr) {
                let backup_path = path_append(&efi_bckup_dir, &msw_boot_mgr.file_name().unwrap());
                info!(
                    "backing up  '{}' to '{}'",
                    &msw_boot_mgr.display(),
                    backup_path.display()
                );
                rename(&msw_boot_mgr, &backup_path).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "Failed to create EFI backup for '{}'",
                        msw_boot_mgr.display()
                    ),
                ))?;
            } else {
                info!(
                    "not backing up  '{}' , file not found",
                    &msw_boot_mgr.display()
                );
            }

            // TODO: allow 32 bit
            let def_boot_mgr = path_append(efi_path.get_path(), EFI_DEFAULT_BOOTMGR64);
            if file_exists(&def_boot_mgr) {
                let backup_path = path_append(&efi_bckup_dir, &def_boot_mgr.file_name().unwrap());
                info!(
                    "backing up  '{}' to '{}'",
                    &def_boot_mgr.display(),
                    backup_path.display()
                );
                rename(&def_boot_mgr, &backup_path).context(MigErrCtx::from_remark(
                    MigErrorKind::Upstream,
                    &format!(
                        "Failed to create EFI backup for '{}'",
                        def_boot_mgr.display()
                    ),
                ))?;
            } else {
                info!(
                    "not backing up  '{}' , file not found",
                    &def_boot_mgr.display()
                );
            }

            Ok(())
        } else {
            Err(MigError::from_remark(
                MigErrorKind::InvState,
                "No EFI directory found",
            ))
        }
    }
}
