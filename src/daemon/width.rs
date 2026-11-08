//! Keeps every sidebar at a fixed share of its window's width.

use std::collections::HashMap;

use crate::tmux::PaneInfo;

/// The share of its window a sidebar takes, in percent.
const SHARE: u32 = 25;

/// The share as a size for `split-window -l`.
pub fn split_size() -> String {
    format!("{SHARE}%")
}

/// The width, in columns, of a sidebar in a window `window_width` wide.
pub fn target(window_width: u16) -> u16 {
    let cols = u32::from(window_width) * SHARE / 100;
    u16::try_from(cols).unwrap_or(window_width).max(1)
}

/// What was last asked of each sidebar: (target width, its width when asked).
pub type Requested = HashMap<String, (u16, u16)>;

/// The sidebars in `panes` to resize, with the width each should take.
///
/// A sidebar tmux can't bring to its target (its neighbours already at their minimum) is not
/// asked again until its width or its target changes, so layout-change notifications from the
/// resize itself don't loop. A lone sidebar is about to be closed, and resizing a pane in a
/// zoomed window would unzoom it, so both are skipped.
pub fn resizes(panes: &[PaneInfo], requested: &mut Requested) -> Vec<(String, u16)> {
    requested.retain(|id, _| panes.iter().any(|p| p.is_sidebar && p.id == *id));
    let mut out = Vec::new();
    for p in panes
        .iter()
        .filter(|p| p.is_sidebar && p.window_panes > 1 && !p.window_zoomed)
    {
        let want = target(p.window_width);
        if p.width == want {
            requested.remove(&p.id);
            continue;
        }
        if requested.get(&p.id) == Some(&(want, p.width)) {
            continue;
        }
        requested.insert(p.id.clone(), (want, p.width));
        out.push((p.id.clone(), want));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidebar(id: &str, width: u16, window_width: u16) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            is_sidebar: true,
            window_panes: 2,
            width,
            window_width,
            ..PaneInfo::default()
        }
    }

    #[test]
    fn target_is_a_quarter_of_the_window() {
        assert_eq!(target(200), 50);
        assert_eq!(target(81), 20, "rounds down");
        assert_eq!(target(2), 1, "never zero");
        assert_eq!(target(u16::MAX), 16383, "no overflow");
    }

    #[test]
    fn only_sidebars_off_their_share_are_resized() {
        let mut plain = sidebar("%9", 70, 200);
        plain.is_sidebar = false;
        let panes = [sidebar("%1", 50, 200), sidebar("%2", 70, 200), plain];
        let mut req = Requested::new();
        assert_eq!(resizes(&panes, &mut req), [("%2".to_string(), 50)]);
    }

    #[test]
    fn a_dragged_sidebar_snaps_back_each_time() {
        let mut req = Requested::new();
        assert_eq!(resizes(&[sidebar("%1", 70, 200)], &mut req).len(), 1);
        assert!(resizes(&[sidebar("%1", 50, 200)], &mut req).is_empty());
        assert!(req.is_empty(), "reached: forgotten");
        assert_eq!(
            resizes(&[sidebar("%1", 70, 200)], &mut req),
            [("%1".to_string(), 50)],
            "dragged again to the same width"
        );
    }

    #[test]
    fn an_unreachable_width_is_not_asked_again() {
        let mut req = Requested::new();
        assert_eq!(resizes(&[sidebar("%1", 70, 200)], &mut req).len(), 1);
        assert!(
            resizes(&[sidebar("%1", 70, 200)], &mut req).is_empty(),
            "tmux left it at 70"
        );
        assert_eq!(
            resizes(&[sidebar("%1", 60, 200)], &mut req).len(),
            1,
            "its width moved"
        );
        assert_eq!(
            resizes(&[sidebar("%1", 60, 280)], &mut req),
            [("%1".to_string(), 70)],
            "its window resized"
        );
    }

    #[test]
    fn split_size_matches_the_target() {
        assert_eq!(split_size(), "25%");
    }

    #[test]
    fn lone_and_zoomed_sidebars_are_left_alone() {
        let mut lone = sidebar("%1", 200, 200);
        lone.window_panes = 1;
        let mut zoomed = sidebar("%2", 200, 200);
        zoomed.window_zoomed = true;
        let mut req = Requested::new();
        assert!(
            resizes(&[lone, zoomed], &mut req).is_empty(),
            "a lone sidebar is about to close; resizing would undo a zoom"
        );
    }

    #[test]
    fn closed_sidebars_are_forgotten() {
        let mut req = Requested::new();
        resizes(&[sidebar("%1", 70, 200)], &mut req);
        resizes(&[], &mut req);
        assert!(req.is_empty());
    }
}
