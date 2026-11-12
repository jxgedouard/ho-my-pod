//! HomePods send clock-sync and retransmit requests back to us over UDP, and
//! remote control commands (Siri, touch) over TCP. Windows Firewall blocks
//! that by default (especially on networks marked "Public"), and the receiver
//! then never finishes the setup. These helpers check for, and create
//! (through a UAC prompt the user accepts), an inbound rule limited to this
//! executable and the local subnet.

use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn exe_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn encode_command(script: &str) -> String {
    use base64::Engine as _;
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(&utf16)
}

/// True if enabled inbound allow rules for this executable cover both UDP
/// (clock sync, retransmits) and TCP (remote control commands).
pub fn is_allowed() -> bool {
    let Some(exe) = exe_path() else { return false };
    let script = format!(
        "$r = Get-NetFirewallApplicationFilter -Program {} -ErrorAction SilentlyContinue | Get-NetFirewallRule | \
         Where-Object {{ $_.Enabled -eq 'True' -and $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' }}; \
         $p = @($r | Get-NetFirewallPortFilter | ForEach-Object {{ $_.Protocol }}); \
         if (($p -contains 'Any') -or (($p -contains 'TCP') -and ($p -contains 'UDP'))) {{ 'yes' }} else {{ 'no' }}",
        ps_quote(&exe)
    );
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_command(&script)])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

/// Asks Windows (UAC) to add the rule. Blocks until the prompt is answered.
/// Returns whether the rule now exists.
pub fn request_allow() -> bool {
    let Some(exe) = exe_path() else { return false };
    let name = "Ho My Pod (AirPlay)";
    let inner = format!(
        "Get-NetFirewallRule -DisplayName {n} -ErrorAction SilentlyContinue | Remove-NetFirewallRule; \
         New-NetFirewallRule -DisplayName {n} -Direction Inbound -Program {p} -Action Allow \
         -RemoteAddress LocalSubnet -Profile Any | Out-Null",
        n = ps_quote(name),
        p = ps_quote(&exe)
    );
    let outer = format!(
        "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList \
         '-NoProfile','-NonInteractive','-EncodedCommand','{}'",
        encode_command(&inner)
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_command(&outer)])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    is_allowed()
}
