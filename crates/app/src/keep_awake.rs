// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Keep the Mac from idle-sleeping while a long job runs (#201: a background
//! optimizer search). The display may still sleep and a manual sleep still
//! works. A no-op on other platforms.

/// Holds the "no idle sleep" activity until dropped.
pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    activity: objc2::rc::Retained<
        objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>,
    >,
}

impl KeepAwake {
    /// Begin the activity, named `reason` (shown by `pmset -g assertions`).
    #[cfg(target_os = "macos")]
    pub fn new(reason: &str) -> KeepAwake {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        // `UserInitiatedAllowingIdleSystemSleep` + idle system sleep disabled:
        // a user-started job, the system awake, the display free to sleep.
        let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::IdleSystemSleepDisabled;
        let activity = NSProcessInfo::processInfo()
            .beginActivityWithOptions_reason(options, &NSString::from_str(reason));
        KeepAwake { activity }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn new(_reason: &str) -> KeepAwake {
        KeepAwake {}
    }
}

#[cfg(target_os = "macos")]
impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: `activity` came from `beginActivityWithOptions:reason:` on
        // the same process info and is ended once.
        unsafe { objc2_foundation::NSProcessInfo::processInfo().endActivity(&self.activity) };
    }
}
