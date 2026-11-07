//! E2E coverage for terminal permission responses: the manual sidebar action
//! and the opt-in external Approval Autopilot both send the agent's mapped
//! keystroke sequence straight to the tmux pane without attaching.

use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};
#[cfg(feature = "serve")]
use crate::harness::{pick_free_port, wait_for_port};

fn enable_approval_autopilot(h: &TuiTestHarness) {
    let path = app_dir_in(h.home_path()).join("config.toml");
    let mut config = std::fs::read_to_string(&path).expect("read harness config.toml");
    config.push_str("\n[session]\napproval_autopilot = true\n");
    std::fs::write(path, config).expect("enable approval autopilot");
}

/// Write a fake agent script that prints a static permission-prompt block,
/// then blocks reading exactly one byte from stdin via `dd` (not `read -r`,
/// which would wait for a newline the app never sends). Once a byte arrives,
/// it echoes it in a recognizable, greppable form and sleeps so the pane
/// stays alive for the assertion.
///
/// Returns the script's absolute path, for use with `--cmd-override` rather
/// than relying on `$PATH`: the tmux pane that runs the agent launches it
/// through a login shell (so version-manager PATHs like NVM load), and that
/// shell's own rc files can reorder `$PATH` ahead of a `$PATH`-installed stub,
/// letting a real agent on the host shadow it. An absolute `--cmd-override`
/// path sidesteps PATH lookup entirely.
fn install_fake_prompt(h: &TuiTestHarness, tool: &str) -> std::path::PathBuf {
    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let prompt = dir.join(format!("fake-{tool}"));
    // The pane's tty starts in canonical line-buffered mode, so a bare
    // digit with no trailing Enter (exactly what `send_key_tokens` sends)
    // sits in the kernel tty driver's line buffer forever; `dd` would
    // never see it without a newline. Disable icanon/echo first so a
    // single byte is delivered to `dd` as soon as it arrives, matching
    // how a real full-screen TUI (raw mode) actually reads a keypress.
    let script = if tool == "codex" {
        "#!/bin/sh\n\
echo 'Would you like to run the following command?'\n\
echo '  $ true'\n\
echo '› 1. Yes, proceed (y)'\n\
echo \"  2. Yes, and don't ask again for this command (p)\"\n\
echo '  3. No, and tell Codex what to do differently (esc)'\n\
stty -icanon -echo 2>/dev/null\n\
first=$(dd bs=1 count=1 2>/dev/null)\n\
second=$(dd bs=1 count=1 2>/dev/null | od -An -t u1 | tr -d '[:space:]')\n\
stty icanon echo 2>/dev/null\n\
echo \"GOT:[$first,$second]\"\n\
sleep 60\n"
    } else {
        "#!/bin/sh\n\
echo 'Do you want to proceed?'\n\
echo '❯ 1. Yes'\n\
echo \"2. Yes, and don't ask again\"\n\
echo '3. No'\n\
echo 'Esc to cancel · Tab to amend'\n\
stty -icanon -echo 2>/dev/null\n\
char=$(dd bs=1 count=1 2>/dev/null)\n\
stty icanon echo 2>/dev/null\n\
echo \"GOT:[$char]\"\n\
sleep 60\n"
    };
    std::fs::write(&prompt, script).expect("write fake prompt script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&prompt, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake prompt script");
    }
    prompt
}

#[cfg(feature = "serve")]
fn wait_for_agent_pane_text(h: &TuiTestHarness, needle: &str) {
    let socket = h.home_path().join("tmux.sock");
    let sessions = std::process::Command::new("tmux")
        .args([
            "-S",
            socket.to_str().unwrap(),
            "list-sessions",
            "-F",
            "#{session_name}",
        ])
        .output()
        .expect("list harness tmux sessions");
    assert!(
        sessions.status.success(),
        "list harness tmux sessions failed"
    );
    let name = String::from_utf8_lossy(&sessions.stdout)
        .lines()
        .next()
        .expect("launched agent tmux session")
        .to_string();

    // A full e2e shard can compile/start several harnesses concurrently; the
    // daemon's two-second status cadence is deterministic, but process and
    // file-watcher startup can be starved on a loaded CI runner.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        let capture = std::process::Command::new("tmux")
            .args([
                "-S",
                socket.to_str().unwrap(),
                "capture-pane",
                "-p",
                "-t",
                &name,
                "-S",
                "-50",
            ])
            .output()
            .expect("capture agent pane");
        if String::from_utf8_lossy(&capture.stdout).contains(needle) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("agent pane did not contain {needle:?} before timeout");
}

#[test]
#[parallel]
fn test_approval_autopilot_allows_without_opening_dialog() {
    require_tmux!();

    let mut h = TuiTestHarness::new("approval_autopilot_allow");
    enable_approval_autopilot(&h);
    h.install_path_command("codex");
    let fake_codex = install_fake_prompt(&h, "codex");

    let project = h.project_path();
    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "ApprovalAutopilot",
        "--tool",
        "codex",
        "--cmd-override",
        fake_codex.to_str().unwrap(),
        "--launch",
    ]);
    assert!(
        output.status.success(),
        "aoe add --launch should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("ApprovalAutopilot");
    // No key is sent to AoE. Its existing background status observation
    // verifies the prompt, selects Codex's persistent choice, and confirms it.
    h.wait_for("GOT:[p,10]");
}

#[test]
#[parallel]
#[cfg(feature = "serve")]
fn test_daemon_reloads_approval_autopilot_and_allows_without_tui() {
    require_tmux!();

    let mut h = TuiTestHarness::new("approval_autopilot_daemon");
    h.stop_daemon_on_drop();
    let fake_claude = install_fake_prompt(&h, "claude");
    let project = h.project_path();
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "ApprovalDaemon",
        "--tool",
        "claude",
        "--cmd-override",
        fake_claude.to_str().unwrap(),
        "--launch",
    ]);
    assert!(add.status.success(), "aoe add --launch should succeed");

    let port = pick_free_port();
    let port_string = port.to_string();
    let serve = h.run_cli(&["serve", "--daemon", "--port", &port_string, "--no-auth"]);
    assert!(
        serve.status.success(),
        "aoe serve --daemon failed: {}",
        String::from_utf8_lossy(&serve.stderr)
    );
    assert!(
        wait_for_port(port, std::time::Duration::from_secs(10)),
        "daemon never bound its test port"
    );

    // The daemon started with the default-off gate. This peer write exercises
    // its config subscription; no TUI is running to perform the approval.
    // Port readiness can precede the asynchronously spawned file-watch task
    // being scheduled on a contended full e2e shard, so let that startup task
    // arm before producing the single edge this test is meant to observe.
    std::thread::sleep(std::time::Duration::from_secs(1));
    enable_approval_autopilot(&h);
    wait_for_agent_pane_text(&h, "GOT:[2]");

    let stop = h.run_cli(&["serve", "--stop"]);
    assert!(stop.status.success(), "aoe serve --stop should succeed");
}

#[test]
#[parallel]
fn test_respond_to_permission_allow_sends_bare_digit() {
    require_tmux!();

    let mut h = TuiTestHarness::new("perm_resp_allow");
    let fake_claude = install_fake_prompt(&h, "claude");

    let project = h.project_path();
    // `--launch` starts the tmux session (spawning the fake claude script)
    // and skips the interactive terminal-attach step, since `run_cli` has no
    // controlling terminal; the exit status is asserted to cover that.
    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "PermRespTest",
        "--tool",
        "claude",
        "--cmd-override",
        fake_claude.to_str().unwrap(),
        "--launch",
    ]);
    assert!(
        output.status.success(),
        "aoe add --launch should succeed without a controlling terminal: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("PermRespTest");
    // The fake claude's static prompt block should already be visible in
    // the attached-by-default preview / pane.
    h.wait_for("Do you want to proceed?");

    // Open the respond-to-permission dialog for the selected session.
    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");

    // Direct mnemonic 'a' = Allow, mirroring structured_view's a/A/d
    // convention. Submits immediately (no Enter needed).
    h.send_keys("a");

    // The shim should have received exactly the bare digit "1" (claude's
    // PermissionResponse.allow), with nothing else injected before or
    // after it.
    h.wait_for("GOT:[1]");
}

#[test]
#[parallel]
fn test_respond_to_permission_deny_sends_bare_digit() {
    require_tmux!();

    let mut h = TuiTestHarness::new("perm_resp_deny");
    let fake_claude = install_fake_prompt(&h, "claude");

    let project = h.project_path();
    // See the allow test above: the exit status now IS asserted.
    let output = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "PermRespDeny",
        "--tool",
        "claude",
        "--cmd-override",
        fake_claude.to_str().unwrap(),
        "--launch",
    ]);
    assert!(
        output.status.success(),
        "aoe add --launch should succeed without a controlling terminal: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("PermRespDeny");
    h.wait_for("Do you want to proceed?");

    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("d");

    h.wait_for("GOT:[3]");
}

#[test]
#[parallel]
fn test_respond_to_codex_permission_allow_always_selects_and_confirms() {
    require_tmux!();

    let mut h = TuiTestHarness::new("perm_resp_codex_allow_always");
    h.install_path_command("codex");
    let fake_codex = install_fake_prompt(&h, "codex");

    let project = h.project_path();
    h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "CodexPermResp",
        "--tool",
        "codex",
        "--cmd-override",
        fake_codex.to_str().unwrap(),
        "--launch",
    ]);

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("CodexPermResp");
    h.wait_for("Would you like to run the following command?");

    h.send_keys("a");
    h.wait_for("Respond to Permission Prompt");
    h.send_keys("A");

    h.wait_for("GOT:[p,10]");
}
