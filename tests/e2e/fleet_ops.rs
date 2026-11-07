//! Fleet operations view: the keyboard-only open/close path through a real
//! terminal, in both hotkey modes.

use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[parallel]
fn test_fleet_view_opens_and_closes_with_the_keyboard() {
    require_tmux!();

    let mut h = TuiTestHarness::new("fleet_open");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-f");
    h.wait_for("Fleet operations");
    // Every panel is reachable without a mouse.
    h.assert_screen_contains("Attention");
    h.assert_screen_contains("Conflicts");
    h.assert_screen_contains("Checkpoints");

    h.send_keys("Escape");
    h.wait_for_absent("Fleet operations", Duration::from_secs(5));
}

#[test]
#[parallel]
fn test_fleet_view_filter_owns_printable_keys() {
    require_tmux!();

    let mut h = TuiTestHarness::new("fleet_filter");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-f");
    h.wait_for("Fleet operations");

    // `q` would quit the home view; inside the filter it is just text.
    h.send_keys("/");
    h.type_text("q");
    std::thread::sleep(Duration::from_millis(200));
    h.assert_screen_contains("Fleet operations");

    // Esc clears the filter, a second Esc closes the panel.
    h.send_keys("Escape");
    std::thread::sleep(Duration::from_millis(100));
    h.assert_screen_contains("Fleet operations");
    h.send_keys("Escape");
    h.wait_for_absent("Fleet operations", Duration::from_secs(5));
}

#[test]
#[parallel]
fn test_checkpoint_label_editor_and_background_failure_are_visible() {
    require_tmux!();

    let mut h = TuiTestHarness::new("fleet_checkpoint");
    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "FleetCheck"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("FleetCheck");
    h.send_keys("C-f");
    h.wait_for("Fleet operations");
    // Left wraps from Attention to Checkpoints and starts the lazy history load.
    h.send_keys("Left");
    h.send_keys("c");
    h.type_text("jk label");
    h.wait_for("label: jk label");
    h.send_keys("Enter");

    // The fixture is intentionally not a Git repository. The worker reports
    // that specific capture failure without closing or freezing the dialog.
    h.wait_for("incomplete");
    h.assert_screen_contains("not a git repository");
    h.assert_screen_contains("Fleet operations");
}
