//! Is the user at the computer and not busy with a fullscreen app?

#[cfg(windows)]
pub fn idle_secs() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut info) == 0 {
            return 0;
        }
        (GetTickCount().wrapping_sub(info.dwTime) / 1000) as u64
    }
}

/// Fullscreen game/video, presentation mode, or "busy" (fullscreen app).
#[cfg(windows)]
pub fn busy() -> bool {
    use windows_sys::Win32::UI::Shell::SHQueryUserNotificationState;
    let mut state = 0;
    unsafe {
        if SHQueryUserNotificationState(&mut state) != 0 {
            return false;
        }
    }
    // QUNS_BUSY = 2, QUNS_RUNNING_D3D_FULL_SCREEN = 3, QUNS_PRESENTATION_MODE = 4
    matches!(state, 2..=4)
}

#[cfg(not(windows))]
pub fn idle_secs() -> u64 {
    0
}

#[cfg(not(windows))]
pub fn busy() -> bool {
    false
}
