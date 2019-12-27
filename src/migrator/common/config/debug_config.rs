use std::path::PathBuf;

use super::MigMode;
use crate::common::MigError;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct DebugConfig {
    // flash on this device instead of / device
    force_flash_device: Option<PathBuf>,
    // pretend mode, stop after unmounting former root
    no_flash: Option<bool>,
    // free form debug parameters, eg. dump-efi
    hacks: Option<Vec<String>>,
}

impl<'a> DebugConfig {
    pub fn default() -> DebugConfig {
        DebugConfig {
            force_flash_device: None,
            // TODO: default to false when project is mature
            no_flash: None,
            hacks: None,
        }
    }

    pub fn is_no_flash(&self) -> bool {
        if let Some(val) = self.no_flash {
            val
        } else {
            // TODO: change to false when mature
            false
        }
    }

    pub fn get_hacks(&'a self) -> Option<&'a Vec<String>> {
        if let Some(ref val) = self.hacks {
            Some(val)
        } else {
            None
        }
    }

    pub fn get_hack(&'a self, param: &str) -> Option<&'a String> {
        if let Some(ref hacks) = self.hacks {
            if let Some(hack) = hacks
                .iter()
                .find(|hack| (hack.as_str() == param) || hack.starts_with(&format!("{}:", param)))
            {
                Some(hack)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn check(&self, _mig_mode: &MigMode) -> Result<(), MigError> {
        // TODO: implement
        Ok(())
    }
}
