//! Easy Install for Windows: an `autounattend.xml` that installs Windows from its ISO
//! with no questions, and the steps that get Setup going.

use std::fs::File;
use std::io::Read;
use std::path::Path;

const TEMPLATE: &str = include_str!("autounattend.xml");
/// How `sources/lang.ini` on a Windows ISO starts.
const LANG_INI: &[u8] = b"[Available UI Languages]";

/// Microsoft's published generic installation keys: they install an edition, and never
/// activate it. Windows 10 and 11 share them.
pub const EDITIONS: [(&str, &str); 6] = [
    ("Windows 11 Pro", "VK7JG-NPHTM-C97JM-9MPGT-3V66T"),
    ("Windows 11 Home", "YTMG3-N6DKC-DKB77-7M9GH-8HVX7"),
    ("Windows 11 Enterprise", "XGVPP-NMH47-7TTHJ-W3FW7-8HV2C"),
    ("Windows 10 Pro", "VK7JG-NPHTM-C97JM-9MPGT-3V66T"),
    ("Windows 10 Home", "YTMG3-N6DKC-DKB77-7M9GH-8HVX7"),
    ("Windows 10 Enterprise", "XGVPP-NMH47-7TTHJ-W3FW7-8HV2C"),
];

/// The generic key for an edition named as Setup names it (`Windows 11 Pro`).
pub fn generic_key(image: &str) -> Option<&'static str> {
    EDITIONS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(image))
        .map(|(_, key)| *key)
}

/// The windows ISO's boot loader waits a few seconds for this before it gives up on
/// the disc.
pub const CD_PROMPT: &str = "Press any key to boot from CD or DVD";

/// What an unattended install does.
pub struct Setup<'a> {
    /// The image in `install.wim`, as Setup names it: `Windows 11 Pro`.
    pub image: &'a str,
    pub product_key: &'a str,
    /// Setup's language, like `en-US`: the ISO's ([`iso_language`]).
    pub language: &'a str,
    pub computer_name: &'a str,
    /// Windows' time zone name, like `W. Europe Standard Time`.
    pub time_zone: &'a str,
    /// The local administrator Setup creates.
    pub user: &'a str,
    pub password: &'a str,
    /// Log on as the user by itself this many times (0: never).
    pub auto_logons: u32,
    /// Commands run as SYSTEM in the specialize pass: (description, command line).
    pub specialize: &'a [(&'a str, &'a str)],
    /// Commands run at the first logon: (description, command line).
    pub first_logon: &'a [(&'a str, &'a str)],
}

/// `text` as XML character data.
pub fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A name Windows takes as a computer name: at most 15 letters, digits and hyphens.
pub fn computer_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    let mut short: String = trimmed.chars().take(15).collect();
    while short.ends_with('-') {
        short.pop();
    }
    if short.is_empty() || short.chars().all(|c| c.is_ascii_digit()) {
        "VMMC-PC".into()
    } else {
        short.to_ascii_uppercase()
    }
}

pub fn answer_file(setup: &Setup) -> String {
    // After BypassNRO, which is command 1.
    let specialize: String = setup
        .specialize
        .iter()
        .enumerate()
        .map(|(index, (description, command))| {
            format!(
                "        <RunSynchronousCommand wcm:action=\"add\">\n          <Order>{}</Order>\n          <Description>{}</Description>\n          <Path>{}</Path>\n        </RunSynchronousCommand>\n",
                index + 2,
                xml_text(description),
                xml_text(command)
            )
        })
        .collect();
    let auto_logon = if setup.auto_logons == 0 {
        String::new()
    } else {
        format!(
            "      <AutoLogon>\n        <Enabled>true</Enabled>\n        <LogonCount>{}</LogonCount>\n        <Username>{}</Username>\n        <Password>\n          <Value>{}</Value>\n          <PlainText>true</PlainText>\n        </Password>\n      </AutoLogon>\n",
            setup.auto_logons,
            xml_text(setup.user),
            xml_text(setup.password)
        )
    };
    let first_logon = if setup.first_logon.is_empty() {
        String::new()
    } else {
        let commands: String = setup
            .first_logon
            .iter()
            .enumerate()
            .map(|(index, (description, command))| {
                format!(
                    "        <SynchronousCommand wcm:action=\"add\">\n          <Order>{}</Order>\n          <Description>{}</Description>\n          <CommandLine>{}</CommandLine>\n        </SynchronousCommand>\n",
                    index + 1,
                    xml_text(description),
                    xml_text(command)
                )
            })
            .collect();
        format!("      <FirstLogonCommands>\n{commands}      </FirstLogonCommands>\n")
    };
    TEMPLATE
        .replace("\r\n", "\n")
        .replace("@@IMAGE@@", &xml_text(setup.image))
        .replace("@@KEY@@", &xml_text(setup.product_key))
        .replace("@@LANGUAGE@@", &xml_text(setup.language))
        .replace("@@COMPUTER@@", &xml_text(setup.computer_name))
        .replace("@@TIMEZONE@@", &xml_text(setup.time_zone))
        .replace("@@USER@@", &xml_text(setup.user))
        .replace("@@PASSWORD@@", &xml_text(setup.password))
        .replace("@@SPECIALIZE@@", &specialize)
        .replace("@@AUTOLOGON@@", &auto_logon)
        .replace("@@FIRSTLOGON@@", &first_logon)
}

/// The first language `lang.ini` lists, as a tag like `de-DE`.
pub fn parse_lang_ini(text: &str) -> Option<String> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    if lines.next()? != "[Available UI Languages]" {
        return None;
    }
    let tag = lines
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| {
            let (tag, index) = line.split_once('=')?;
            index.trim().parse::<u32>().ok()?;
            Some(tag.trim())
        })
        .filter(|tag| !tag.is_empty())?;
    Some(match tag.split_once('-') {
        Some((language, region)) if region.len() == 2 => {
            format!("{}-{}", language.to_lowercase(), region.to_uppercase())
        }
        _ => tag.to_owned(),
    })
}

/// The language of a Windows ISO's Setup, from its `sources/lang.ini`. The ISO's files
/// are on a UDF file system, and a file that small sits inside its file entry, so
/// rather than read UDF, look for the file's contents.
pub fn iso_language(iso: &Path) -> Result<String, String> {
    let mut file = File::open(iso).map_err(|e| format!("can't open {}: {e}", iso.display()))?;
    // Each read keeps the end of the last one, so a match can straddle two reads.
    const KEEP: usize = 512;
    let mut buffer = vec![0u8; KEEP + (8 << 20)];
    let mut kept = 0;
    loop {
        let read = file
            .read(&mut buffer[kept..])
            .map_err(|e| format!("can't read {}: {e}", iso.display()))?;
        if read == 0 {
            return Err(format!(
                "{}: no sources/lang.ini found; is it a Windows ISO?",
                iso.display()
            ));
        }
        let filled = kept + read;
        let mut from = 0;
        while let Some(at) = buffer[from..filled]
            .windows(LANG_INI.len())
            .position(|window| window == LANG_INI)
        {
            let start = from + at;
            let end = (start + 4096).min(filled);
            let text = String::from_utf8_lossy(&buffer[start..end]);
            if let Some(language) = parse_lang_ini(&text) {
                return Ok(language);
            }
            from = start + 1;
        }
        let keep = KEEP.min(filled);
        buffer.copy_within(filled - keep..filled, 0);
        kept = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup<'a>(first_logon: &'a [(&'a str, &'a str)]) -> Setup<'a> {
        Setup {
            image: "Windows 11 Pro",
            product_key: generic_key("windows 11 pro").unwrap(),
            language: "de-DE",
            computer_name: "WIN11",
            time_zone: "W. Europe Standard Time",
            user: "Anna & Bo",
            password: "p<w>d",
            auto_logons: 1,
            specialize: &[(
                "Load test-signed drivers",
                "bcdedit /set {current} testsigning on",
            )],
            first_logon,
        }
    }

    #[test]
    fn fills_in_everything_as_xml() {
        let xml = answer_file(&setup(&[("Report", "cmd /c echo hi > x")]));
        assert!(!xml.contains("@@"), "every placeholder filled");
        assert!(xml.contains("<Value>Windows 11 Pro</Value>"));
        assert!(xml.contains("<Key>VK7JG-NPHTM-C97JM-9MPGT-3V66T</Key>"));
        assert!(xml.contains("<Name>Anna &amp; Bo</Name>"));
        assert!(xml.contains("<Value>p&lt;w&gt;d</Value>"));
        assert!(xml.contains("<Order>2</Order>\n          <Description>Load test-signed drivers"));
        assert!(xml.contains("<CommandLine>cmd /c echo hi &gt; x</CommandLine>"));
        assert!(xml.contains("<LogonCount>1</LogonCount>"));
        assert!(xml.contains("<TimeZone>W. Europe Standard Time</TimeZone>"));
    }

    #[test]
    fn leaves_out_what_isnt_asked_for() {
        let mut quiet = setup(&[]);
        quiet.auto_logons = 0;
        let xml = answer_file(&quiet);
        assert!(!xml.contains("AutoLogon"));
        assert!(!xml.contains("FirstLogonCommands"));
    }

    #[test]
    fn specialize_commands_follow_bypass_nro_and_every_element_closes() {
        let xml = answer_file(&setup(&[]));
        assert_eq!(xml.matches("<RunSynchronousCommand").count(), 2);
        assert!(xml.find("BypassNRO").unwrap() < xml.find("Load test-signed drivers").unwrap());
        assert_eq!(xml.matches("de-DE").count(), 9);
        assert!(!xml.contains("LabConfig"), "no hardware check bypasses");
        for tag in [
            "settings",
            "component",
            "RunSynchronous",
            "DiskConfiguration",
            "UserAccounts",
        ] {
            assert_eq!(
                xml.matches(&format!("<{tag}>")).count() + xml.matches(&format!("<{tag} ")).count(),
                xml.matches(&format!("</{tag}>")).count(),
                "{tag}"
            );
        }
    }

    #[test]
    fn computer_names_fit_windows() {
        assert_eq!(computer_name("Windows 11"), "WINDOWS-11");
        assert_eq!(
            computer_name("my very long machine name"),
            "MY-VERY-LONG-MA"
        );
        assert_eq!(computer_name("ü"), "VMMC-PC");
        assert_eq!(computer_name("123"), "VMMC-PC");
    }

    #[test]
    fn reads_the_language_from_lang_ini() {
        let ini = "[Available UI Languages]\r\nde-de = 3\r\n\r\n[Fallback Languages]\r\nde-de = en-us\r\n";
        assert_eq!(parse_lang_ini(ini).as_deref(), Some("de-DE"));
        assert_eq!(
            parse_lang_ini("[Available UI Languages]\nsr-latn-rs = 1\n").as_deref(),
            Some("sr-latn-rs")
        );
        assert_eq!(
            parse_lang_ini("[Available UI Languages]\n[Other]\nx = 1"),
            None
        );
    }
}
