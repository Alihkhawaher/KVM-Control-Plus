//! How a captured session is ended locally.
//!
//! Kept in its own module so both the settings layer and the Win32 hook can use
//! it without dragging the whole input module in.

/// Which gesture releases input capture and returns control to the local desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LeaveGesture {
    /// Middle mouse button — the original Control3 gesture.
    Middle = 0,
    /// `Ctrl+Alt+Backspace`.
    CtrlAltBackspace = 1,
    /// No local gesture: use the on-screen button or the panic hotkey.
    None = 2,
}

impl LeaveGesture {
    pub const ALL: [LeaveGesture; 3] = [
        LeaveGesture::Middle,
        LeaveGesture::CtrlAltBackspace,
        LeaveGesture::None,
    ];

    pub fn from_u8(v: u8) -> LeaveGesture {
        match v {
            1 => LeaveGesture::CtrlAltBackspace,
            2 => LeaveGesture::None,
            _ => LeaveGesture::Middle,
        }
    }

    /// Stable token used in the settings file.
    pub fn as_str(self) -> &'static str {
        match self {
            LeaveGesture::Middle => "middle",
            LeaveGesture::CtrlAltBackspace => "ctrl-alt-backspace",
            LeaveGesture::None => "none",
        }
    }

    /// Lenient inverse of [`LeaveGesture::as_str`].
    pub fn parse(s: &str) -> LeaveGesture {
        match s.trim().to_ascii_lowercase().as_str() {
            "ctrl-alt-backspace" | "ctrl+alt+backspace" | "backspace" => {
                LeaveGesture::CtrlAltBackspace
            }
            "none" | "off" | "disabled" => LeaveGesture::None,
            _ => LeaveGesture::Middle,
        }
    }

    /// Label for the settings drop-down.
    pub fn label(self) -> &'static str {
        match self {
            LeaveGesture::Middle => "Middle mouse button",
            LeaveGesture::CtrlAltBackspace => "Ctrl + Alt + Backspace",
            LeaveGesture::None => "Disabled (button / hotkey only)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_u8() {
        for g in LeaveGesture::ALL {
            assert_eq!(LeaveGesture::from_u8(g as u8), g);
        }
        assert_eq!(LeaveGesture::from_u8(200), LeaveGesture::Middle);
    }

    #[test]
    fn round_trips_through_text() {
        for g in LeaveGesture::ALL {
            assert_eq!(LeaveGesture::parse(g.as_str()), g);
        }
        assert_eq!(
            LeaveGesture::parse("CTRL-ALT-BACKSPACE"),
            LeaveGesture::CtrlAltBackspace
        );
        assert_eq!(LeaveGesture::parse("nonsense"), LeaveGesture::Middle);
    }
}
