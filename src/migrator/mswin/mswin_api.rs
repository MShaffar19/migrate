use std::path::{Path};

use crate::{
    defs::{OSArch},
    common::{
        os_api::OSApi,
        MigError,
        device_info::DeviceInfo,
        path_info::PathInfo,
    },
    mswin::wmi_utils::WMIOSInfo,
};
use crate::mswin::wmi_utils::WmiUtils;

struct MSWinApi<'a> {
    os_info: &'a WMIOSInfo,
}

impl MSWinApi {
    pub fn new(os_info: &WMIOSInfo) -> Result<MSWinApi, MigError> {
        Ok(MSWinApi{ os_info})
    }
}

impl OSApi for MSWinApi {
    fn get_os_arch(&self) -> Result<OSArch, MigError> {
        Ok(self.os_info.os_arch.clone())
    }

    fn get_os_name(&self) -> Result<String, MigError>;

    fn path_info_from_path<P: AsRef<Path>>(&self, path: P) -> Result<PathInfo, MigError>;
    fn device_info_from_partition<P: AsRef<Path>>(
        &self,
        partition: P,
    ) -> Result<DeviceInfo, MigError>;
    fn expect_type<P: AsRef<Path>>(&self, file: P, ftype: &FileType) -> Result<(), MigError>;

}