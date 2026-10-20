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

#[cfg(target_os = "macos")]
pub fn idle_secs() -> u64 {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
    }
    // kCGEventSourceStateCombinedSessionState = 0, kCGAnyInputEventType = ~0
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) };
    if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    }
}

/// Not detected on macOS yet: popups also appear over fullscreen apps.
#[cfg(target_os = "macos")]
pub fn busy() -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn idle_secs() -> u64 {
    0
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn busy() -> bool {
    false
}
