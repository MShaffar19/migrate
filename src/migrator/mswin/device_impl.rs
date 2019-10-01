use crate::common::MigErrorKind;
use crate::{
    common::{
        device::Device, migrate_info::MigrateInfo, stage2_config::Stage2ConfigBuilder, Config,
        MigError,
    },
    defs::OSArch,
    mswin::{device_impl::intel_nuc::IntelNuc, powershell::is_secure_boot, win_api::is_efi_boot},
};

mod intel_nuc;

pub fn get_device(
    mig_info: &MigrateInfo,
    config: &Config,
    stage2_config: &mut Stage2ConfigBuilder,
) -> Result<Box<Device>, MigError> {
    match mig_info.os_arch {
        OSArch::AMD64 => Ok(Box::new(IntelNuc::from_config(
            mig_info,
            config,
            stage2_config,
        )?)),
        _ => Err(MigError::from_remark(
            MigErrorKind::InvParam,
            &format!(
                "Only AMD64 architecture devices are currently supported on '{}'",
                mig_info.os_name
            ),
        )),
    }
}