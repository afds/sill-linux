//! A trackpad gesture from a device (kind 28) as GNOME's own touchpad gestures, from GNOME's state
//! (the extension reads it and acts; this decides). Pure.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Overview {
    Hidden,
    Windows,
    Apps,
}

impl Overview {
    pub fn parse(s: &str) -> Option<Overview> {
        match s {
            "hidden" => Some(Overview::Hidden),
            "windows" => Some(Overview::Windows),
            "apps" => Some(Overview::Apps),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Overview::Hidden => "hidden",
            Overview::Windows => "windows",
            Overview::Apps => "apps",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Overview(Overview),
    /// The workspace that many to the right.
    Workspace(i32),
    ShowDesktop,
    /// Bring back the windows a show-desktop minimized.
    WindowsBack,
    Nothing,
}

/// - swipe up: Activities, then the app grid;
/// - swipe down: back the same way;
/// - swipe left and right: the next and previous workspace (fingers moving left bring the
///   workspace on the right, as on the Mac);
/// - pinch: the app grid, or closes it;
/// - spread: closes Activities, else shows the desktop;
/// - a pinch or a spread while the desktop is shown brings its windows back.
pub fn decide(gesture: &str, overview: Overview, desktop_shown: bool) -> Action {
    use Overview::*;
    if desktop_shown && matches!(gesture, "pinch" | "spread") {
        return Action::WindowsBack;
    }
    match (gesture, overview) {
        ("swipeUp", Hidden) => Action::Overview(Windows),
        ("swipeUp", Windows) => Action::Overview(Apps),
        ("swipeDown", Apps) => Action::Overview(Windows),
        ("swipeDown", Windows) => Action::Overview(Hidden),
        ("swipeLeft", _) => Action::Workspace(1),
        ("swipeRight", _) => Action::Workspace(-1),
        ("pinch", Apps) => Action::Overview(Hidden),
        ("pinch", _) => Action::Overview(Apps),
        ("spread", Hidden) => Action::ShowDesktop,
        ("spread", _) => Action::Overview(Hidden),
        _ => Action::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Overview::*;

    #[test]
    fn as_gnome_does() {
        // The nested shell's run, step by step.
        assert_eq!(decide("swipeUp", Hidden, false), Action::Overview(Windows));
        assert_eq!(decide("swipeUp", Windows, false), Action::Overview(Apps));
        assert_eq!(decide("swipeUp", Apps, false), Action::Nothing);
        assert_eq!(decide("swipeDown", Apps, false), Action::Overview(Windows));
        assert_eq!(decide("swipeDown", Windows, false), Action::Overview(Hidden));
        assert_eq!(decide("swipeDown", Hidden, false), Action::Nothing);
        assert_eq!(decide("pinch", Hidden, false), Action::Overview(Apps));
        assert_eq!(decide("pinch", Apps, false), Action::Overview(Hidden));
        assert_eq!(decide("spread", Windows, false), Action::Overview(Hidden));
        assert_eq!(decide("spread", Hidden, false), Action::ShowDesktop);
        assert_eq!(decide("spread", Hidden, true), Action::WindowsBack);
        assert_eq!(decide("pinch", Hidden, true), Action::WindowsBack);
        assert_eq!(decide("swipeLeft", Hidden, true), Action::Workspace(1));
        assert_eq!(decide("swipeRight", Apps, false), Action::Workspace(-1));
        assert_eq!(decide("bogus", Hidden, false), Action::Nothing);
    }
}
