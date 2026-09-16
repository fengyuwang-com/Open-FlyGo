// Central helper for spawning console children (powershell / cmd / wsl / reg /
// netsh / taskkill ...) from the GUI panel without flashing a black console
// window on the desktop.
//
// Why: every console-subsystem child spawned from the tray/GUI panel process
// gets a new visible console unless CREATE_NO_WINDOW (0x0800_0000) is passed.
// `-WindowStyle Hidden` alone is unreliable (the console is created before the
// PowerShell engine applies the style, so it can still flash), and it is not
// available at all for non-PowerShell children. CREATE_NO_WINDOW is the only
// reliable, per-child switch.
//
// Elevated spawns (UAC "runas") go through ShellExecuteW, which accepts no
// creation flags; those sites (tray.rs, api/access.rs) pass SW_HIDE +
// -WindowStyle Hidden instead. SMB lifecycle spawns use a hand-rolled
// CreateProcessW with SW_HIDE + CREATE_NO_WINDOW (smb.rs). Everything else
// must spawn through this module.
//
// Windows-only by design (the whole panel binary targets Windows).

use std::os::windows::process::CommandExt;
use std::process::Command;

/// CREATE_NO_WINDOW: the child gets a console (so console apps behave
/// normally) but it is never shown. Same value as
/// windows::Win32::System::Threading::CREATE_NO_WINDOW.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Command preconfigured with CREATE_NO_WINDOW so console children never
/// flash a window. Args/stdio/behavior are unchanged from plain
/// `Command::new`; callers chain exactly as before.
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal must keep matching Win32 CREATE_NO_WINDOW (0x0800_0000);
    /// a typo here would make every console child flash a visible window.
    /// Compared against the windows-crate constant so the check is load-bearing.
    #[test]
    fn create_no_window_matches_win32_value() {
        assert_eq!(
            CREATE_NO_WINDOW,
            windows::Win32::System::Threading::CREATE_NO_WINDOW.0
        );
    }

    /// hidden_command only pre-sets the creation flags; program and args
    /// stay exactly as the caller chains them.
    #[test]
    fn hidden_command_keeps_program_and_args() {
        let mut cmd = hidden_command("powershell.exe");
        cmd.arg("-NoProfile").arg("-Command").arg("echo hi");
        assert_eq!(cmd.get_program().to_string_lossy(), "powershell.exe");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-NoProfile", "-Command", "echo hi"]);
    }
}
