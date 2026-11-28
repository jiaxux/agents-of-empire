//! Kitty Graphics Protocol renderer for real brand logos in the TUI sidebar.
//!
//! Embeds 48x48 transparent PNG logos (pre-encoded as base64) for supported
//! agents so Ghostty, Kitty, WezTerm, and SSH/tmux sessions with passthrough
//! render actual brand icons right before each chat name. Unsupported
//! terminals, headless tests, and agents without a bundled PNG fall back
//! cleanly to 1-cell Unicode icons.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Dedicated high Kitty image ID range so sidebar icons never collide with
/// other graphics clients (e.g. yazi previews).
const ICON_ID_BASE: u32 = 0x00A0_E001;
const ICON_COUNT: u32 = 9;

/// Number of terminal columns reserved for a square brand icon (`c=2, r=1`).
pub(crate) const KITTY_ICON_COLS: usize = 2;

/// Tracks whether this process ever transmitted Kitty sidebar icons so
/// `TerminalGuard::drop` only emits cleanup sequences when needed.
static KITTY_ICONS_USED: AtomicBool = AtomicBool::new(false);
static KITTY_IN_TMUX: AtomicBool = AtomicBool::new(false);

/// One visible sidebar row requesting a Kitty brand logo at `(x, y)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentIconSlot {
    /// 0-based screen column of the 2-cell logo slot.
    pub x: u16,
    /// 0-based screen row of the 2-cell logo slot.
    pub y: u16,
    /// Stable Kitty image ID for the agent logo.
    pub icon_id: u32,
    /// Row highlight state (0 = normal, 1 = hovered, 2 = selected) so changing
    /// row background color forces a fresh placement over the repainted cells.
    pub highlight: u8,
}

const ICON_ANTIGRAVITY_ID: u32 = 0x00A0E001;
const ICON_ANTIGRAVITY_PNG_B64: &str = "\
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";

const ICON_CLAUDE_ID: u32 = 0x00A0E002;
const ICON_CLAUDE_PNG_B64: &str = "\
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";

const ICON_CODEX_ID: u32 = 0x00A0E003;
const ICON_CODEX_PNG_B64: &str = "\
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\
rkJggg==";

const ICON_GEMINI_ID: u32 = 0x00A0E004;
const ICON_GEMINI_PNG_B64: &str = "\
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\
3OjxP7ztcLuDAZgnAAAAAElFTkSuQmCC";

const ICON_OPENCODE_ID: u32 = 0x00A0E005;
const ICON_OPENCODE_PNG_B64: &str = "\
iVBORw0KGgoAAAANSUhEUgAAADAAAAAwCAYAAABXAvmHAAAAZ0lEQVR42u3ZwQmAIBiAUZVoxwZxigZpxy65QYEiP+H7\
zh58KAqakrR2+WvAdd5P5ASPur/Osfx9BQAAAABi22af07PvGVsIAAAAAAAAAAAAAAAAAAAAoKvhd6Ho/wNbCAAAAEDS\
QA1tWQt/opfvqAAAAABJRU5ErkJggg==";

const ICON_COPILOT_ID: u32 = 0x00A0E006;
const ICON_COPILOT_PNG_B64: &str = "\
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\
gg==";

const ICON_CURSOR_ID: u32 = 0x00A0E007;
const ICON_CURSOR_PNG_B64: &str = "\
iVBORw0KGgoAAAANSUhEUgAAADAAAAAwCAYAAABXAvmHAAAErklEQVR42tWaXWwUVRTHf2dnt3VLq2AUgjbdCmhrFxTD\
bpFIbKq+GeVFNFHExBiQGPlSkQQSQEVRg7aICRITIw+SgA8+EBMSExJjSNndigVnWyhpO7sEE1RCaG3tdneOD+2a0tJ2\
v7v9P87Mvff87se5Z869Qh4UCoVcWnb3GyhvAqByUP69etDn8w3lui3JdYXBsPUUyufA4jENdSZsti5f4jlRlAAt4ej9\
htp7gdVTfPqTwuZ6r8csCoCzZ7tnx0sd21E2A6UpFhsCvhGna4ev5p6/pgVAVR2h9sgalE+AeRlWc02F93oerDr4vEii\
YACB9miD2HYTsDRHM7DdobJl2eKqk3kFaGm7XGk4Ex8Ca/LhAFQ4IXE2+R/ydOUUIBS6UqbuoW3ANsBNfhVTOBQzBnau\
rK3tzQpAVaU1HHlO4VPAQ2F1Bdjjq6v6WkTstAFa261ltk0z8BjTKSGkCd1Uv6T6dEoAoXDPfFXZDbwGOCgOKfC9EH/b\
510YmRAgYPa8JMghoJziVJ+g633e6u/GAZw6pc7yuZFrQIXCSYEbRWb8XUAj0Nt3terOxkaJAziTb13zL7hJuCtGqAZJ\
sD0dd5ZvmaZZ0k/5daDCXXmpjJEOnmiOP4tBR8C0mls6O28vBoB/mLXxVi58skXqEthoxEo6gqa1TlWnbUG3tV2aK8jO\
W71Lxaj5wFehcCQQMq2V0wEw5HJ9ANyRKUBSyxR+DprWsV/DXQXb1AJm91JVXp3ofbrTQoDVCTXCQdPafToazXdYgSBN\
gJErgKTKgF2uG/bFYNhaq6qSl94PR14AaZjsm2wXZiXKt6GwdSpgdi/NpfGno1G3Q3XfVN/lyLNIg+BoDZrWkZbzXfNy\
UaPzhv2OQnWBAP6v62XDYXSEfrfe7ezsLM249y9E75Xh0J1CAiQ1W4V912MlbYFw5PFMKiiJ2x8Ds6YLIKkaUf0h3UJn\
2ntWKLyYzrDnU3PSTRQ4bGlK53e1WOJ9AILh6CtAfboLryj0S0dHhaB7M/EcRaFS271jJO6aeQDBc9aCkcweM3MEDD4j\
9bRkcQG0mj1PAKuy2T2nTcdUDXs42mRGAtwXtjYAS2YkwLlz1hyQXbkIwIZ/2/6oGQB6CwUwaLBnJFWSrnoHLi/qHwfQ\
2ChxVNYXAiJgWl5gQwZF+wR9PZkTGjeF/IurjopoDXAYSOSRYT+jclKphEnAcSHuHZ2Vg6mTu01AVpkIv9dzUxtBM7IK\
0opSg2rr5pSTu2Oiw6zT66MBhrNrs86DPJBC0ZTS65N6IRFRn9dzXAZcdaJsB/qyGY0BLd+SgvExhQODxkCt3+s5PJnx\
U47AWGVyxJQcgba2S3NjTtdFJkhQwfARkzp04/La6u687AOPPlx52e/1rBUcK0DPpFM25iz5aBLjf1ORhvo6zzPpGJ/2\
CIz9e0rlmNXv9UjQ7HkEJHSLDvtbhfezOWbNeCcWEdtf5zlShnsRsAcYnMgRKNI8pq0hhQNyW3xhfZ2nOVPjsxqBcb6u\
o6uGhLEfePpmANaJcHjUox8xElv9tQsu5CQjlYdd9kmBJsZc9gC9aNvyVtFe9hitcddthC+k/88v83Hd5j/4q8HGFsh7\
UgAAAABJRU5ErkJggg==";

const ICON_KIMI_ID: u32 = 0x00A0E008;
const ICON_KIMI_PNG_B64: &str = "\
iVBORw0KGgoAAAANSUhEUgAAADAAAAAwCAYAAABXAvmHAAADsUlEQVR42u2YTWxUZRSGn/Pd6TSGxMgCWkln7vgX6FxA\
6swUQqxRB5C4cWlYYNyZmBjdsFBECqKJCRsXLjRxA6QxJv7ExASEYo0uamuLQ71d2AoOYqBtTJs0Cp3OvceFtUHD3JnO\
T5lJ5uzm3jPnPO+85/vunU9ogFh3VHuMcBzYBkwB7095vEOv5E3dw7+pe40wAHQDYSACHGuzOAkgdU2vKm1vcQklViAj\
XdcObDhCJAAehZ76HiGLuwLvC38aGjduonzWqAIWUV6aPiS/hAplDLvZb4A1/7vstXrs3brVni2n64ULl+/Jh80JYEPh\
qeC7pGO/AvCXx1SrxUeAdUvClC98OHNQfgQIBfR77HYX8yHvbmC2TPgzS9thoZhTzIvLH3plDtgXVHdVRmgJ/qui8Cp7\
Uk5kaCW1zSrCp4rBd2+ODq+0fk0FXLyYXZtvNWeLwM/6vuwuB77YGqgYfsHiLEoiCN4Ydqec6Ei5fWriwOjEtXULIR2A\
IHidMb55PNFpj1TSK1QLeC+X6wfZEgQvxkonnMhYpf2q6kAmM7n+H3i2BLy/TIux0snOyuGr6kAmM7k+F2rpBzYHwouk\
k52Rn6rVtyoCBscuteWM1Q84AWlTQDoVj7rVdN1UAz5krPPF4BXS3Y7tVnvNVeTAkHu53WD6FeJB8B48ucOxx2ux45lK\
4AVzvgj89VrCl+3AYOZqh+B9DTwYkHbVYJ5IOZHJWj7tV+yA54d2WiFvoAg8HjyVqDF8eSMk2gc8UIK1BzKZ62vqT0Cp\
BwrwfC60MDLs/trVkAKWYiPI98NutldVa9JLAv5SapV7nRPR55Lx2LW6c0Dg9xLSdqnK6A9jV/bUlQBRfWN+OhoDjgB+\
kfR2NXp6yM2+67pu+I6PkIgeSsZjx5a/M57dhXISaC+h94gnZt+OeGTizjgg8vqt8ACpuH0unF98WOFMCRUSlvqZofHs\
y6vugKIHup3Y8UL3P1a17nezr6nI4f+c6RT+MT71xby6vbPj51UQIAdTTvTtUoqPuFd6fLQP6CghPQ98gvCl1RI+/chD\
987UZITyxusrNTfhRL/1jbUN+KLE97JnUU54i7nP6+ZcaHtnxx8px34GeAFYKPEx3lI3ApYXuGN/IKI7BSaqWXdVT6eT\
8djoTetGAjjVkAIAHt20aT7l2PsV9itMN5yAf6PbsU+15PyNAkcrERIkYP52m5Dk9Ua1RHR13TeXdOzDa8O5KOjTiLyn\
yCAiv9GMZjSjGc1oRgnxNwbJTLqLNSQqAAAAAElFTkSuQmCC";

const ICON_QWEN_ID: u32 = 0x00A0E009;
const ICON_QWEN_PNG_B64: &str = "\
iVBORw0KGgoAAAANSUhEUgAAADAAAAAwCAYAAABXAvmHAAAFEklEQVR42s2ZW2wUZRTHf2fa7s4UKDeVAJqgFrHtbgsx\
YPqimKghYmLk8qLhxXg3BRrUSCAaozEIChgvTybGhBjxQR9MJMGImCCXyK27XYobUCMGMQYwUHZn2905PnSBlr10v93Z\
bk+yLzsz3/zP+c453//8R/DDmjXoOGzxYJXAlGFXLgNfukI3EblCFazej0WcRjar0iW5lyYBz9iK48KqajgglS4Q6NA2\
y+N4CcFY7EblJ78dsCpewGNbKTupwnZWat24csAJ6TLgoZK2Wpnv9LFi/KTQHLXtScSAOwye2iXwmQE6Ty2OuMflD98d\
sEO6HuEdqm8ZVbpTvfKhbw40LtBZXppfgYmMjWUyHvcMxqTHlxrwMmwaQ/AAdXUW233ZAadDF6nHQT9asHHBKsuTvfJ1\
RTugHmtrAT7bitf6kUKzqZ3NLkolGlt0ptdAKzoswsJpNyK/Xw8DexDuqwl8ZU/BGnBCuk6FTXlOVRX4KBmV1Vni1mQ7\
xIEZYww/KR53J2PyZ04KOWHtVGFzAUogCl12SIfI2Cm5hLKhBvHfciP47A6oZYc5CCwcZYGzrsc8YtKffWYf0DlG4E+7\
Ado5IomcGrDbWYWOCh5gli2sd2EDiFdfr4+l02wFHgSCVQI+KMohtViTDzyA2GGNAa0lLugitI4o6hqbBdxuQoHUYwvj\
yMQO6/fZNDBpZw+4vbLXbzCNC3QWaWYOC6+XUOLFxlFLhI2AGnnt+3Ci4oT0Ey/NGQ8OX/t5HLWVs067Li/oQDIihxB2\
mG0AHcGTPO0X/GCYZ1V4oQAzaFJlRzCkdxYkcxNCOiMjxIEmg/deCHjMvRSTC5WAnxzWqSkljnDTKEH7JhWVZXm50JVe\
+Yehg8zEpqUsNlYa/QHljdHAZyP9eDCkDxem080atB16gWaD96c9i/kDPRIrS9EIaYsl9AANJT5ywp1OB3slnctGT0lK\
hNdMdSXLyz9olNTDha0G4AFag+d5ruhAY4d1d6lKw7D8XJqKyndGhRvSR0X4tgy/LzYMMvfySTmfdx7wLLqBtKG08QHN\
WjqdaNOACO+VuXFT0wFeLzjQDPRIDOFTw0WbHYeXSh5LhS5gXtmnhvJiIKzhgjPxxAV682Ca+A1C7Whp9F9DPXf1H5N/\
i903sV1vyShxhckVNrBdblQeyTtSDqZZKGbFhcCUdJq3Rm1byts+gAdYMiGkM3J2oKlNpw1YxIHpfuo3ABPadH7G4jDg\
Cw0R4bacHRi0eLNM8EX1G4DM0DW/ONT+ZET+GuFAoEPbFJ6vcOHF+ciXE9aVwP3+KSy8nNOFLI/V+PDRQ6F7xB+d6ijG\
VKWYfZGMyoEcB8QvzUdHrmP3sw6Y4xP4c1aaV/LqQsAeYKkPL/nxWuq0662qxhTlHPCxCPeqYl9vYPRZyrZEn/ydn0p0\
qmP3c6LCaLnU0XJV07fDugN40nCNp9yolPQdYWQXOiBJMY9Wjn5zFbwT1k7gCcP6OeZG+dxkqB8pf/XyFfBDmeB/c4V3\
s1AsVbZjJgSrZbEaxCuZDucT4oLoyhS8DyyBazlY9IAV4ZAqa64O4Fm9aZFh8e9M9sg+QwZQHTPUmwAS4tGSTz40SiEf\
zawRSH7ts5YO/Gxw7xm3obyDrmoOWEPjaUnFKMKrhbTPmjmQiMhRtCS9aX8yws6yA1XFFMJqYD1wscgtg5bQNfQJYhw6\
kDgmZ8ViCfDLDXO2B/SpsCIRkaOVvON/qCW4BPjnZ+sAAAAASUVORK5CYII=";

/// Return the Kitty image ID for a tool if we ship an embedded PNG logo for it.
pub(crate) fn icon_id_for_tool(tool: &str) -> Option<u32> {
    match tool.trim() {
        "antigravity" => Some(ICON_ANTIGRAVITY_ID),
        "claude" => Some(ICON_CLAUDE_ID),
        "codex" => Some(ICON_CODEX_ID),
        "gemini" => Some(ICON_GEMINI_ID),
        "opencode" => Some(ICON_OPENCODE_ID),
        "copilot" => Some(ICON_COPILOT_ID),
        "cursor" => Some(ICON_CURSOR_ID),
        "kimi" => Some(ICON_KIMI_ID),
        "qwen" => Some(ICON_QWEN_ID),
        _ => None,
    }
}

fn png_b64_for_id(id: u32) -> Option<&'static str> {
    match id {
        ICON_ANTIGRAVITY_ID => Some(ICON_ANTIGRAVITY_PNG_B64),
        ICON_CLAUDE_ID => Some(ICON_CLAUDE_PNG_B64),
        ICON_CODEX_ID => Some(ICON_CODEX_PNG_B64),
        ICON_GEMINI_ID => Some(ICON_GEMINI_PNG_B64),
        ICON_OPENCODE_ID => Some(ICON_OPENCODE_PNG_B64),
        ICON_COPILOT_ID => Some(ICON_COPILOT_PNG_B64),
        ICON_CURSOR_ID => Some(ICON_CURSOR_PNG_B64),
        ICON_KIMI_ID => Some(ICON_KIMI_PNG_B64),
        ICON_QWEN_ID => Some(ICON_QWEN_PNG_B64),
        _ => None,
    }
}

/// Stateful Kitty Graphics Protocol driver for sidebar brand logos.
///
/// Uploads each PNG at most once per terminal session and diffs placements on
/// every frame so steady-state typing in live-send emits zero graphics bytes.
pub(crate) struct KittyIconRenderer {
    enabled: bool,
    in_tmux: bool,
    uploaded_ids: HashSet<u32>,
    last_slots: Vec<AgentIconSlot>,
}

impl KittyIconRenderer {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            in_tmux: false,
            uploaded_ids: HashSet::new(),
            last_slots: Vec::new(),
        }
    }

    pub(crate) fn detect() -> Self {
        if cfg!(test) {
            return Self::disabled();
        }
        let in_tmux = std::env::var_os("TMUX").is_some();
        let enabled = detect_kitty_graphics_support(in_tmux);
        if enabled {
            KITTY_IN_TMUX.store(in_tmux, Ordering::Relaxed);
        }
        Self {
            enabled,
            in_tmux,
            uploaded_ids: HashSet::new(),
            last_slots: Vec::new(),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Force re-placement on the next frame (e.g. after `clear_terminal` or resize).
    pub(crate) fn invalidate(&mut self) {
        self.last_slots.clear();
    }

    /// Reconcile on-screen Kitty placements against `slots`.
    ///
    /// Must be called inside the frame synchronized-update bracket right after
    /// `terminal.draw(...)`. Emits nothing when `slots` is identical to the
    /// previous frame.
    pub(crate) fn sync<W: Write>(
        &mut self,
        writer: &mut W,
        slots: &[AgentIconSlot],
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.last_slots == slots {
            return Ok(());
        }

        // Upload any logo PNG not yet cached in the terminal.
        for slot in slots {
            if !self.uploaded_ids.contains(&slot.icon_id) {
                if let Some(b64) = png_b64_for_id(slot.icon_id) {
                    write_upload(writer, self.in_tmux, slot.icon_id, b64)?;
                    self.uploaded_ids.insert(slot.icon_id);
                    KITTY_ICONS_USED.store(true, Ordering::Relaxed);
                }
            }
        }

        // Delete previous placements while keeping uploaded image data cached
        // (`d=I`, uppercase I).
        let mut cleared_ids = HashSet::new();
        for prev in &self.last_slots {
            if cleared_ids.insert(prev.icon_id) {
                write_kitty_apc(
                    writer,
                    self.in_tmux,
                    &format!("a=d,d=I,i={},q=2", prev.icon_id),
                )?;
            }
        }

        if !slots.is_empty() {
            // Save cursor (`DECSC`) so placements do not disturb live-send caret.
            writer.write_all(b"\x1b7")?;
            for (idx, slot) in slots.iter().enumerate() {
                let placement_id = idx + 1;
                write!(writer, "\x1b[{};{}H", slot.y + 1, slot.x + 1)?;
                write_kitty_apc(
                    writer,
                    self.in_tmux,
                    &format!(
                        "a=p,i={},p={},c={},r=1,C=1,q=2",
                        slot.icon_id, placement_id, KITTY_ICON_COLS
                    ),
                )?;
            }
            writer.write_all(b"\x1b8")?;
        }

        self.last_slots.clear();
        self.last_slots.extend_from_slice(slots);
        Ok(())
    }

    /// Delete all on-screen placements AND free cached icon images in the
    /// terminal (used when suspending to `tmux attach` or exiting).
    pub(crate) fn clear_all<W: Write>(&mut self, writer: &mut W) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        for id in self.uploaded_ids.drain() {
            write_kitty_apc(writer, self.in_tmux, &format!("a=d,d=i,i={},q=2", id))?;
        }
        self.last_slots.clear();
        Ok(())
    }
}

/// Best-effort cleanup called from `TerminalGuard::drop` so exiting aoe never
/// leaves orphaned sidebar logos behind.
pub(crate) fn cleanup_on_exit<W: Write>(writer: &mut W) {
    if !KITTY_ICONS_USED.swap(false, Ordering::Relaxed) {
        return;
    }
    let in_tmux = KITTY_IN_TMUX.load(Ordering::Relaxed);
    for offset in 0..ICON_COUNT {
        let id = ICON_ID_BASE + offset;
        let _ = write_kitty_apc(writer, in_tmux, &format!("a=d,d=i,i={},q=2", id));
    }
}

fn detect_kitty_graphics_support(in_tmux: bool) -> bool {
    if !io::stdout().is_terminal() {
        return false;
    }
    if let Ok(val) = std::env::var("AOE_KITTY_ICONS") {
        let v = val.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "0" | "false" | "off" | "no") {
            return false;
        }
        if matches!(v.as_str(), "1" | "true" | "on" | "yes") {
            return !in_tmux || ensure_tmux_passthrough();
        }
    }

    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let supported = std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || std::env::var_os("GHOSTTY_BIN_DIR").is_some()
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("KITTY_PID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("WEZTERM_PANE").is_some()
        || matches!(
            term_program.as_str(),
            "ghostty" | "kitty" | "wezterm" | "konsole"
        )
        || term.contains("ghostty")
        || term.contains("kitty");

    if !supported {
        return false;
    }
    if in_tmux {
        return ensure_tmux_passthrough();
    }
    true
}

fn ensure_tmux_passthrough() -> bool {
    let check = |flag: &str| {
        std::process::Command::new("tmux")
            .args(["show-options", flag, "allow-passthrough"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                let val = s.trim();
                val == "on" || val == "all" || val.ends_with(" on") || val.ends_with(" all")
            })
            .unwrap_or(false)
    };
    if check("-wv") || check("-gv") {
        return true;
    }
    // Enable passthrough only on the window hosting aoe (does not alter global
    // tmux defaults) and verify tmux accepted it.
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-q", "-w", "allow-passthrough", "on"])
        .status();
    check("-wv") || check("-gv")
}

fn write_kitty_apc<W: Write>(writer: &mut W, in_tmux: bool, body: &str) -> io::Result<()> {
    if in_tmux {
        write!(writer, "\x1bPtmux;\x1b\x1b_G{body}\x1b\x1b\\\x1b\\")
    } else {
        write!(writer, "\x1b_G{body}\x1b\\")
    }
}

fn write_upload<W: Write>(writer: &mut W, in_tmux: bool, id: u32, b64: &str) -> io::Result<()> {
    const CHUNK: usize = 4096;
    let bytes = b64.as_bytes();
    if bytes.len() <= CHUNK {
        return write_kitty_apc(writer, in_tmux, &format!("a=t,f=100,i={id},q=2;{b64}"));
    }
    let mut offset = 0;
    let mut first = true;
    while offset < bytes.len() {
        let end = (offset + CHUNK).min(bytes.len());
        let more = if end < bytes.len() { 1 } else { 0 };
        let chunk = &b64[offset..end];
        if first {
            write_kitty_apc(
                writer,
                in_tmux,
                &format!("a=t,f=100,i={id},m={more},q=2;{chunk}"),
            )?;
            first = false;
        } else {
            write_kitty_apc(writer, in_tmux, &format!("m={more},q=2;{chunk}"))?;
        }
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_logos_have_valid_png_base64_headers() {
        for tool in [
            "antigravity",
            "claude",
            "codex",
            "gemini",
            "opencode",
            "copilot",
            "cursor",
            "kimi",
            "qwen",
        ] {
            let id = icon_id_for_tool(tool).expect("tool should have icon id");
            let b64 = png_b64_for_id(id).expect("icon id should have PNG data");
            // Standard PNG header `\x89PNG\r\n\x1a\n` encodes to `iVBORw0KGgo`.
            assert!(
                b64.starts_with("iVBORw0KGgo"),
                "{tool} logo must be valid PNG base64"
            );
        }
        assert_eq!(icon_id_for_tool("unknown-custom"), None);
    }

    #[test]
    fn sync_uploads_once_and_skips_unchanged_frames() {
        let mut renderer = KittyIconRenderer {
            enabled: true,
            in_tmux: false,
            uploaded_ids: HashSet::new(),
            last_slots: Vec::new(),
        };
        let slots = vec![
            AgentIconSlot {
                x: 4,
                y: 2,
                icon_id: ICON_ANTIGRAVITY_ID,
                highlight: 2,
            },
            AgentIconSlot {
                x: 4,
                y: 3,
                icon_id: ICON_CLAUDE_ID,
                highlight: 0,
            },
        ];

        let mut buf = Vec::new();
        renderer.sync(&mut buf, &slots).unwrap();
        let first_out = String::from_utf8(buf).unwrap();
        assert!(first_out.contains(&format!("a=t,f=100,i={ICON_ANTIGRAVITY_ID}")));
        assert!(first_out.contains(&format!("a=t,f=100,i={ICON_CLAUDE_ID}")));
        assert!(first_out.contains(&format!("a=p,i={ICON_ANTIGRAVITY_ID},p=1,c=2,r=1,C=1,q=2")));
        assert!(first_out.contains(&format!("a=p,i={ICON_CLAUDE_ID},p=2,c=2,r=1,C=1,q=2")));

        // Identical frame emits zero bytes (live-send steady state).
        let mut buf2 = Vec::new();
        renderer.sync(&mut buf2, &slots).unwrap();
        assert!(buf2.is_empty());

        // Highlight change deletes placements with uppercase d=I (preserving
        // uploaded images) and does not re-upload PNG data.
        let mut moved = slots.clone();
        moved[0].highlight = 0;
        moved[1].highlight = 2;
        let mut buf3 = Vec::new();
        renderer.sync(&mut buf3, &moved).unwrap();
        let third_out = String::from_utf8(buf3).unwrap();
        assert!(!third_out.contains("a=t,"));
        assert!(third_out.contains(&format!("a=d,d=I,i={ICON_ANTIGRAVITY_ID},q=2")));
        assert!(third_out.contains(&format!("a=p,i={ICON_CLAUDE_ID},p=2,c=2,r=1,C=1,q=2")));
    }

    #[test]
    fn tmux_mode_wraps_kitty_apc_in_dcs_passthrough() {
        let mut buf = Vec::new();
        write_kitty_apc(&mut buf, true, "a=d,d=I,i=1,q=2").unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i=1,q=2\x1b\x1b\\\x1b\\"
        );
    }
}
