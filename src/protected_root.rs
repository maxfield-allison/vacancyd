//! Path containment for Windows setup and service state writes restricted
//! to the protected `ProgramData\vacancyd` folder.
//!
//! Judging an arbitrary path left one way after another for an unelevated
//! program to redirect it: a junction removed by resolving the path, or a
//! directory that became a junction after it was checked. A single folder
//! that only Administrators and LocalSystem can change removes that class.
//! This module compares paths as written, without touching the disk, and is
//! kept platform-neutral so every CI runner tests it. `windows_acl` finds
//! the folder and inspects what is actually there.

/// Whether the absolute Windows path `path` is `root` or inside it, compared
/// as written: drive letter and names without regard to case, and a `\\?\`
/// prefix ignored. Only a local drive path is accepted. A `.` or `..` name,
/// a `/`, an empty name or a `:` after the drive (an alternate data stream)
/// makes the path not inside, as does a short 8.3 spelling of the root.
/// Resolving links is deliberately not part of the comparison: a path that
/// is inside as written must also lead there, which the caller proves by
/// refusing every reparse point on the way.
pub fn contains(root: &str, path: &str) -> bool {
    match (names(root), names(path)) {
        (Some(root), Some(path)) => path.starts_with(&root),
        _ => false,
    }
}

/// The drive and each name of a local drive path, lowercased, or `None`
/// when the path is in any other form.
fn names(path: &str) -> Option<Vec<String>> {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let drive = path.get(..3)?;
    let mut letters = drive.chars();
    let (letter, colon, separator) = (letters.next()?, letters.next()?, letters.next()?);
    if !letter.is_ascii_alphabetic() || colon != ':' || separator != '\\' {
        return None;
    }
    let mut names = vec![letter.to_ascii_lowercase().to_string()];
    let rest = path[3..].strip_suffix('\\').unwrap_or(&path[3..]);
    if rest.is_empty() {
        return Some(names);
    }
    for name in rest.split('\\') {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', ':']) {
            return None;
        }
        names.push(name.to_lowercase());
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::contains;

    const ROOT: &str = r"C:\ProgramData\vacancyd";

    #[test]
    fn the_root_and_paths_below_it_are_inside() {
        for path in [
            r"C:\ProgramData\vacancyd",
            r"C:\ProgramData\vacancyd\",
            r"C:\ProgramData\vacancyd\vacancyd.admission",
            r"C:\ProgramData\vacancyd\cache\pressure",
            r"c:\programdata\VACANCYD\logs",
            r"\\?\C:\ProgramData\vacancyd\vacancyd.admission",
        ] {
            assert!(contains(ROOT, path), "{path}");
        }
        assert!(contains(r"\\?\C:\ProgramData\vacancyd", ROOT));
    }

    #[test]
    fn paths_beside_above_or_outside_the_root_are_not_inside() {
        for path in [
            r"C:\ProgramData",
            r"C:\ProgramData\vacancyd-old",
            r"C:\ProgramData\vacancydx\logs",
            r"C:\ProgramData\other\vacancyd",
            r"C:\Temp\vacancyd",
            r"C:\Windows\Temp\vacancyd",
            r"D:\ProgramData\vacancyd",
            r"C:\",
        ] {
            assert!(!contains(ROOT, path), "{path}");
        }
    }

    #[test]
    fn spellings_that_could_lead_elsewhere_are_not_inside() {
        for path in [
            // Names Windows would resolve after the comparison.
            r"C:\ProgramData\vacancyd\..\other",
            r"\\?\C:\ProgramData\vacancyd\..\..\Users\owner",
            r"C:\ProgramData\vacancyd\.\logs",
            r"C:\ProgramData\vacancyd\\logs",
            r"C:\ProgramData\vacancyd/logs",
            // An alternate data stream, and a short name for ProgramData.
            r"C:\ProgramData\vacancyd\logs:stream",
            r"C:\PROGRA~3\vacancyd\logs",
            // Network, device and relative forms.
            r"\\server\share\ProgramData\vacancyd",
            r"\\?\UNC\server\share\ProgramData\vacancyd",
            r"\\.\C:\ProgramData\vacancyd",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\ProgramData\vacancyd",
            r"ProgramData\vacancyd",
            r"C:ProgramData\vacancyd",
            "",
        ] {
            assert!(!contains(ROOT, path), "{path}");
        }
    }

    #[test]
    fn a_root_that_is_not_a_local_drive_path_contains_nothing() {
        assert!(!contains("", r"C:\ProgramData\vacancyd"));
        assert!(!contains(r"ProgramData\vacancyd", r"ProgramData\vacancyd"));
    }
}
