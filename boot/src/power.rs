use harlan_hal::PowerControl;
use uefi::Status;
use uefi::runtime::{self, ResetType};

/// UEFI Runtime Services are available both before and after
/// `ExitBootServices`, so this works unchanged regardless of when Fase 2+
/// eventually calls it.
pub struct UefiPower;

impl PowerControl for UefiPower {
    fn reboot(&self) -> ! {
        runtime::reset(ResetType::COLD, Status::SUCCESS, None)
    }

    fn shutdown(&self) -> ! {
        runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None)
    }
}
