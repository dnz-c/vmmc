//! The paths in a transfer, which come from the other side and so are checked before
//! anything is made from them: names a Windows folder can hold, nothing that climbs out
//! of the drag's folder or names a device.

/// The deepest a path goes.
pub const MAX_DEPTH: usize = 64;
/// The longest name, in UTF-16 units as Windows counts them.
const MAX_NAME: usize = 255;

/// The names in `path` (`/` between them), if every one passes [`check_name`] and there
/// are at most [`MAX_DEPTH`].
pub fn check(path: &str) -> Result<Vec<&str>, String> {
    let names: Vec<&str> = path.split('/').collect();
    if names.len() > MAX_DEPTH {
        return Err(format!("{path:?} is too deep"));
    }
    for name in &names {
        check_name(name).map_err(|e| format!("{path:?}: {e}"))?;
    }
    Ok(names)
}

/// Whether `name` is a plain file or folder name on Windows and Linux alike.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("{name:?} isn't a name"));
    }
    if name.encode_utf16().count() > MAX_NAME {
        return Err("a name is too long".into());
    }
    if let Some(c) = name.chars().find(|&c| !allowed(c)) {
        return Err(format!("{name:?} has {c:?} in it"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!("{name:?} ends in a dot or a space"));
    }
    if reserved(name) {
        return Err(format!("{name:?} names a device"));
    }
    Ok(())
}

/// `name` made into one that passes [`check_name`]: what Windows can't hold becomes `_`.
/// For the names a Linux guest sends.
pub fn fix_name(name: &str) -> String {
    let mut fixed: String = name
        .chars()
        .map(|c| if allowed(c) { c } else { '_' })
        .collect();
    while fixed.ends_with('.') || fixed.ends_with(' ') {
        fixed.pop();
        fixed.push('_');
    }
    while fixed.encode_utf16().count() > MAX_NAME {
        fixed.pop();
    }
    if fixed.is_empty() || fixed == "." || fixed == ".." || reserved(&fixed) {
        fixed.insert(0, '_');
    }
    fixed
}

fn allowed(c: char) -> bool {
    !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

/// Device names Windows reserves, with or without an extension (and spaces before it).
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let Some(number) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    let mut chars = number.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('0'..='9' | '¹' | '²' | '³'), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_pass() {
        assert_eq!(check("a.txt").unwrap(), ["a.txt"]);
        assert_eq!(
            check("Fotos/2026 Sommer/Strand (1).jpg").unwrap(),
            ["Fotos", "2026 Sommer", "Strand (1).jpg"]
        );
        assert!(check("ünïcødé ✓/日本語.txt").is_ok());
        assert!(check(".hidden").is_ok());
        assert!(check("CONFIG.SYS").is_ok());
        assert!(check("COM10").is_ok());
    }

    #[test]
    fn escapes_and_devices_fail() {
        for bad in [
            "",
            "/etc/passwd",
            "a//b",
            "..",
            "a/../../b",
            "./a",
            r"..\..\Windows",
            r"C:\Windows",
            "C:",
            "a:stream",
            "NUL",
            "nul.txt",
            "com1",
            "LPT9.log",
            "CONOUT$",
            "trailing.",
            "trailing ",
            "tab\there",
            "new\nline",
            "nul\0byte",
            "what?",
            "a*",
            "pipe|",
            "quote\"",
            "<angle>",
        ] {
            assert!(check(bad).is_err(), "{bad:?} passed");
        }
        assert!(check(&"x".repeat(256)).is_err());
        assert!(check(&["d"; MAX_DEPTH + 1].join("/")).is_err());
        assert!(check(&["d"; MAX_DEPTH].join("/")).is_ok());
    }

    #[test]
    fn fixed_names_pass() {
        for name in [
            "a:b",
            "back\\slash",
            "NUL",
            "con.txt",
            "..",
            ".",
            "",
            "dots...",
            "space ",
            "ok.txt",
            "tab\t",
        ] {
            let fixed = fix_name(name);
            assert!(check_name(&fixed).is_ok(), "{name:?} became {fixed:?}");
        }
        assert_eq!(fix_name("ok.txt"), "ok.txt");
        assert_eq!(fix_name("a:b"), "a_b");
        assert_eq!(fix_name("NUL"), "_NUL");
        assert!(check_name(&fix_name(&"é".repeat(300))).is_ok());
    }
}
