// Which details panel is open. At most one of the image's technical
// details, the selected target's details and the protected devices is open
// at a time: opening one closes the others, which keeps the window short.
// Presentation only.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    ImageDetails,
    TargetDetails,
    ProtectedDevices,
}

// The open panel after `panel` was opened (`expanded`) or closed.
// Closing a panel that is not the open one (it is being closed because
// another one opened) changes nothing.
pub fn after_toggle(open: Option<Panel>, panel: Panel, expanded: bool) -> Option<Panel> {
    match (expanded, open) {
        (true, _) => Some(panel),
        (false, Some(current)) if current == panel => None,
        (false, current) => current,
    }
}

// Whether `panel` is shown expanded.
pub fn is_open(open: Option<Panel>, panel: Panel) -> bool {
    open == Some(panel)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANELS: [Panel; 3] = [
        Panel::ImageDetails,
        Panel::TargetDetails,
        Panel::ProtectedDevices,
    ];

    #[test]
    fn opening_a_panel_closes_the_others() {
        let open = after_toggle(None, Panel::ImageDetails, true);
        assert_eq!(open, Some(Panel::ImageDetails));

        let open = after_toggle(open, Panel::ProtectedDevices, true);
        assert_eq!(open, Some(Panel::ProtectedDevices));
        for panel in PANELS {
            assert_eq!(is_open(open, panel), panel == Panel::ProtectedDevices);
        }

        // The previously open panel reports being closed: nothing changes.
        assert_eq!(
            after_toggle(open, Panel::ImageDetails, false),
            Some(Panel::ProtectedDevices)
        );
    }

    #[test]
    fn closing_the_open_panel_leaves_none_open() {
        let open = after_toggle(None, Panel::TargetDetails, true);
        assert_eq!(after_toggle(open, Panel::TargetDetails, false), None);
        assert_eq!(after_toggle(None, Panel::TargetDetails, false), None);
    }

    #[test]
    fn at_most_one_panel_is_ever_open() {
        let mut open = None;
        for (panel, expanded) in [
            (Panel::ImageDetails, true),
            (Panel::TargetDetails, true),
            (Panel::ImageDetails, false),
            (Panel::ProtectedDevices, true),
            (Panel::TargetDetails, false),
            (Panel::ImageDetails, true),
        ] {
            open = after_toggle(open, panel, expanded);
            assert!(PANELS.iter().filter(|&&p| is_open(open, p)).count() <= 1);
        }
        assert_eq!(open, Some(Panel::ImageDetails));
    }
}
