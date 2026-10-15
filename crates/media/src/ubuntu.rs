//! Easy Install for Ubuntu: autoinstall configuration on a CIDATA disc (which
//! cloud-init reads as a NoCloud data source), and the GRUB steps that start the
//! installer with `autoinstall` on the kernel's command line, so it asks nothing.

use std::fmt::Write as _;
use std::hash::{BuildHasher, RandomState};

use sha2::{Digest, Sha512};

/// The first entry of the ISO's GRUB menu.
pub const GRUB_MENU: &str = "Try or Install Ubuntu";
/// What GRUB's command line runs instead of the menu entry: the same kernel, with
/// autoinstall and a serial console added.
pub const GRUB_COMMANDS: [&str; 4] = [
    "set gfxpayload=keep",
    "linux /casper/vmlinuz autoinstall console=tty0 console=ttyS0,115200 ---",
    "initrd /casper/initrd",
    "boot",
];

/// Typed on the serial console at the first boot: at the GRUB menu, its command line,
/// then the commands. Each pair: wait for the text, then type the bytes.
pub fn install_steps() -> Vec<(String, String)> {
    let mut steps = vec![(GRUB_MENU.to_owned(), "c".to_owned())];
    for command in GRUB_COMMANDS {
        steps.push(("grub>".to_owned(), format!("{command}\r")));
    }
    steps
}

pub struct Setup<'a> {
    pub user: &'a str,
    pub real_name: &'a str,
    pub password: &'a str,
    pub hostname: &'a str,
    /// Like `en_US.UTF-8`.
    pub locale: &'a str,
    /// An X keyboard layout, like `us` or `de`.
    pub keyboard: &'a str,
    /// Like `Europe/Berlin`.
    pub time_zone: &'a str,
}

/// A YAML double-quoted string.
fn yaml(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c < ' ' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A hostname: lowercase letters, digits and hyphens.
pub fn hostname(name: &str) -> String {
    let cleaned: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed: String = cleaned.trim_matches('-').chars().take(63).collect();
    if trimmed.is_empty() {
        "ubuntu".into()
    } else {
        trimmed
    }
}

/// The autoinstall configuration: the whole disk, the user, and a power-off at the
/// end (so the install media can be taken out before the first boot).
pub fn user_data(setup: &Setup) -> String {
    let hash = sha512_crypt(setup.password, &random_salt());
    format!(
        "#cloud-config\n# Made by vmmc's Easy Install.\nautoinstall:\n  version: 1\n  locale: {}\n  keyboard:\n    layout: {}\n  timezone: {}\n  identity:\n    realname: {}\n    hostname: {}\n    username: {}\n    password: {}\n  storage:\n    layout:\n      name: direct\n  shutdown: poweroff\n",
        yaml(setup.locale),
        yaml(setup.keyboard),
        yaml(setup.time_zone),
        yaml(setup.real_name),
        yaml(setup.hostname),
        yaml(setup.user),
        yaml(&hash),
    )
}

/// The NoCloud meta-data.
pub fn meta_data(instance: &str) -> String {
    format!("instance-id: {}\n", yaml(instance))
}

const ITOA64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn random_salt() -> String {
    let state = RandomState::new();
    let mut bits = u128::from(state.hash_one(1u8)) << 64 | u128::from(state.hash_one(2u8));
    (0..16)
        .map(|_| {
            let c = ITOA64[(bits & 63) as usize] as char;
            bits >>= 6;
            c
        })
        .collect()
}

/// SHA-512 crypt (`$6$`), as Linux's /etc/shadow takes it, written from Ulrich
/// Drepper's "Unix crypt using SHA-256 and SHA-512" with its default 5000 rounds.
pub fn sha512_crypt(password: &str, salt: &str) -> String {
    let password = password.as_bytes();
    let salt: Vec<u8> = salt.bytes().take(16).collect();
    let digest = |parts: &[&[u8]]| -> [u8; 64] {
        let mut hasher = Sha512::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize().into()
    };
    let b = digest(&[password, &salt, password]);
    let mut a = Sha512::new();
    a.update(password);
    a.update(&salt);
    let mut left = password.len();
    while left > 64 {
        a.update(b);
        left -= 64;
    }
    a.update(&b[..left]);
    let mut bit = password.len();
    while bit > 0 {
        if bit & 1 == 1 {
            a.update(b);
        } else {
            a.update(password);
        }
        bit >>= 1;
    }
    let mut a: [u8; 64] = a.finalize().into();
    let repeat = |source: &[u8], times: usize| -> [u8; 64] {
        let mut hasher = Sha512::new();
        for _ in 0..times {
            hasher.update(source);
        }
        hasher.finalize().into()
    };
    let dp = repeat(password, password.len());
    let p: Vec<u8> = dp.iter().copied().cycle().take(password.len()).collect();
    let ds = repeat(&salt, 16 + usize::from(a[0]));
    let s: Vec<u8> = ds.iter().copied().cycle().take(salt.len()).collect();
    for round in 0..5000 {
        let mut c = Sha512::new();
        if round % 2 == 1 {
            c.update(&p);
        } else {
            c.update(a);
        }
        if round % 3 != 0 {
            c.update(&s);
        }
        if round % 7 != 0 {
            c.update(&p);
        }
        if round % 2 == 1 {
            c.update(a);
        } else {
            c.update(&p);
        }
        a = c.finalize().into();
    }
    const ORDER: [(usize, usize, usize); 21] = [
        (0, 21, 42),
        (22, 43, 1),
        (44, 2, 23),
        (3, 24, 45),
        (25, 46, 4),
        (47, 5, 26),
        (6, 27, 48),
        (28, 49, 7),
        (50, 8, 29),
        (9, 30, 51),
        (31, 52, 10),
        (53, 11, 32),
        (12, 33, 54),
        (34, 55, 13),
        (56, 14, 35),
        (15, 36, 57),
        (37, 58, 16),
        (59, 17, 38),
        (18, 39, 60),
        (40, 61, 19),
        (62, 20, 41),
    ];
    let mut out = format!("$6${}$", String::from_utf8_lossy(&salt));
    let mut encode = |high: u8, middle: u8, low: u8, chars: usize| {
        let mut word = u32::from(high) << 16 | u32::from(middle) << 8 | u32::from(low);
        for _ in 0..chars {
            out.push(ITOA64[(word & 63) as usize] as char);
            word >>= 6;
        }
    };
    for (high, middle, low) in ORDER {
        encode(a[high], a[middle], a[low], 4);
    }
    encode(0, 0, a[63], 2);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha512_crypt_matches_the_specification() {
        // From the specification's test vectors.
        assert_eq!(
            sha512_crypt("Hello world!", "saltstring"),
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
        // The test VM's password, as mkpasswd made it.
        assert_eq!(
            sha512_crypt("vmmc", "vmmctestsalt"),
            "$6$vmmctestsalt$B7N3MJ/x3lLqhX2rs30Dz8iS8q79bqX3Vw./58DiO4M1XdNPy.Hz40KUmmtfKBq0Ezho5VGSPSZ.QV.eQTMmh/"
        );
    }

    #[test]
    fn user_data_is_autoinstall_with_the_user_in_it() {
        let text = user_data(&Setup {
            user: "anna",
            real_name: "Anna \"A\" Bo",
            password: "secret",
            hostname: &hostname("My Ubuntu!"),
            locale: "de_DE.UTF-8",
            keyboard: "de",
            time_zone: "Europe/Berlin",
        });
        assert!(text.starts_with("#cloud-config\n"));
        assert!(text.contains("    username: \"anna\"\n"));
        assert!(text.contains("    realname: \"Anna \\\"A\\\" Bo\"\n"));
        assert!(text.contains("    hostname: \"my-ubuntu\"\n"));
        assert!(text.contains("    password: \"$6$"));
        assert!(!text.contains("secret"), "only the hash");
        assert!(text.contains("  shutdown: poweroff\n"));
    }

    #[test]
    fn the_install_steps_drive_grub() {
        let steps = install_steps();
        assert_eq!(steps[0], (GRUB_MENU.to_owned(), "c".to_owned()));
        assert_eq!(steps.len(), 5);
        assert!(steps[2].1.contains("autoinstall"));
        assert!(steps[4].1.starts_with("boot"));
    }
}
