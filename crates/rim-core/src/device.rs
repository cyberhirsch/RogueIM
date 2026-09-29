//! Local OS and device-class detection (PRD PR-6, PR-8).

use crate::proto::DeviceClass;

/// Short OS code shown inside the status icon: WIN, MAC, or a Linux distro code.
pub fn os_code() -> String {
    #[cfg(target_os = "windows")]
    {
        "WIN".into()
    }
    #[cfg(target_os = "macos")]
    {
        "MAC".into()
    }
    #[cfg(target_os = "linux")]
    {
        linux_distro_code()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "???".into()
    }
}

#[cfg(target_os = "linux")]
fn linux_distro_code() -> String {
    let id = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("ID=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_default();
    match id.as_str() {
        "ubuntu" => "UBU",
        "debian" => "DEB",
        "fedora" => "FED",
        "arch" => "ARC",
        "linuxmint" => "MNT",
        "opensuse-tumbleweed" | "opensuse-leap" => "SUS",
        "raspbian" => "RPI",
        _ => "TUX",
    }
    .into()
}

/// Desktop vs. laptop; mobile is only ever set by a mobile build.
pub fn device_class() -> DeviceClass {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // BatteryFlag 128 = "no system battery".
        if unsafe { GetSystemPowerStatus(&mut s) } != 0 && s.BatteryFlag != 128 && s.BatteryFlag != 255 {
            return DeviceClass::Laptop;
        }
        DeviceClass::Desktop
    }
    #[cfg(target_os = "linux")]
    {
        // SMBIOS chassis types for portables: 8,9,10,14,31,32.
        let chassis = std::fs::read_to_string("/sys/class/dmi/id/chassis_type")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if matches!(chassis, Some(8 | 9 | 10 | 14 | 31 | 32)) {
            return DeviceClass::Laptop;
        }
        DeviceClass::Desktop
    }
    #[cfg(target_os = "macos")]
    {
        let model = std::process::Command::new("sysctl")
            .args(["-n", "hw.model"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        if model.contains("MacBook") {
            DeviceClass::Laptop
        } else {
            DeviceClass::Desktop
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        DeviceClass::Desktop
    }
}

/// True when a laptop is running on battery.
pub fn on_battery() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        unsafe { GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0 }
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/sys/class/power_supply")
            .map(|rd| {
                rd.flatten().any(|e| {
                    e.file_name().to_string_lossy().starts_with("BAT")
                        && std::fs::read_to_string(e.path().join("status"))
                            .map(|s| s.trim() == "Discharging")
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("Battery Power"))
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        false
    }
}
