//! The host's settings, as defaults for Easy Install: the time zone, the locale and the
//! keyboard layout a new guest should have.

/// The host's time zone, as Windows names it (`W. Europe Standard Time`).
#[cfg(windows)]
pub fn windows_time_zone() -> Option<String> {
    use windows::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation,
    };
    let mut info = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: fills in the struct; TIME_ZONE_ID_INVALID is u32::MAX.
    if unsafe { GetDynamicTimeZoneInformation(&mut info) } == u32::MAX {
        return None;
    }
    let name = &info.TimeZoneKeyName;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let name = String::from_utf16_lossy(&name[..len]);
    (!name.is_empty()).then_some(name)
}

#[cfg(not(windows))]
pub fn windows_time_zone() -> Option<String> {
    None
}

/// The host's time zone as Linux names it (`Europe/Berlin`), for the common zones;
/// otherwise UTC.
pub fn linux_time_zone() -> String {
    windows_time_zone()
        .and_then(|zone| {
            WINDOWS_TO_IANA
                .iter()
                .find(|(windows, _)| *windows == zone)
                .map(|(_, iana)| (*iana).to_owned())
        })
        .unwrap_or_else(|| "Etc/UTC".into())
}

/// The commonest zones of CLDR's windowsZones table (each zone's "001" territory).
const WINDOWS_TO_IANA: [(&str, &str); 44] = [
    ("UTC", "Etc/UTC"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("India Standard Time", "Asia/Calcutta"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("Cen. Australia Standard Time", "Australia/Adelaide"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("Eastern Standard Time", "America/New_York"),
    ("Central Standard Time", "America/Chicago"),
    ("Mountain Standard Time", "America/Denver"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("Canada Central Standard Time", "America/Regina"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("Pacific SA Standard Time", "America/Santiago"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
];

/// The host's locale as Linux names it (`de_DE.UTF-8`).
pub fn linux_locale() -> String {
    #[cfg(windows)]
    {
        let mut name = [0u16; 85];
        // SAFETY: the buffer is LOCALE_NAME_MAX_LENGTH long.
        let len = unsafe { windows::Win32::Globalization::GetUserDefaultLocaleName(&mut name) };
        if len > 1 {
            let tag = String::from_utf16_lossy(&name[..len as usize - 1]);
            if let Some((language, region)) = tag.split_once('-')
                && language.len() == 2
                && region.len() == 2
            {
                return format!("{language}_{region}.UTF-8");
            }
        }
    }
    "en_US.UTF-8".into()
}

/// The host's keyboard layout as an X layout name (`de`), for the common ones.
pub fn keyboard_layout() -> &'static str {
    #[cfg(windows)]
    {
        // SAFETY: plain FFI; the low word of the HKL is the layout's language.
        let layout = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout(0) };
        let language = (layout.0 as usize & 0xffff) as u16;
        if let Some((_, name)) = LAYOUTS.iter().find(|(id, _)| *id == language) {
            return name;
        }
    }
    "us"
}

const LAYOUTS: [(u16, &str); 18] = [
    (0x0409, "us"),
    (0x0809, "gb"),
    (0x0407, "de"),
    (0x0807, "ch"),
    (0x0c07, "at"),
    (0x040c, "fr"),
    (0x080c, "be"),
    (0x0410, "it"),
    (0x0c0a, "es"),
    (0x0413, "nl"),
    (0x0816, "pt"),
    (0x041d, "se"),
    (0x0414, "no"),
    (0x0406, "dk"),
    (0x040b, "fi"),
    (0x0405, "cz"),
    (0x0415, "pl"),
    (0x0411, "jp"),
];

/// The user's name, for the guest's first account.
pub fn user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .filter(|name| !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or_else(|| "user".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_look_right() {
        assert!(linux_locale().ends_with(".UTF-8"));
        assert!(!keyboard_layout().is_empty());
        assert!(linux_time_zone().contains('/'));
        assert!(user_name().chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
