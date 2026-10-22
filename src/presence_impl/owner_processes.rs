//! Owner-process matching for a backend that scans processes itself (Linux).
//!
//! Windows compares the foreground executable's file stem with
//! `owner.processes`, ignoring case. Linux has no service-readable
//! foreground window, so the scan looks at every process of a user whose
//! unlocked session is in the foreground, and recognises each one by three
//! names: the executable file, the kernel's process name, and the first
//! command-line argument. Each is compared with and without its last
//! extension, so `Marathon`, `Marathon.x86_64` and a Proton game's
//! `C:\Games\Marathon.exe` all match `Marathon`.

/// The configured name a process matches, as the process spelled it.
pub fn owner_match<'a>(
    names: impl IntoIterator<Item = &'a str>,
    configured: &[String],
) -> Option<String> {
    if configured.is_empty() {
        return None;
    }
    let wanted = configured
        .iter()
        .map(|name| name.to_lowercase())
        .collect::<Vec<_>>();
    for raw in names {
        let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
        let stem = match base.rfind('.') {
            Some(dot) if dot > 0 => &base[..dot],
            _ => base,
        };
        for candidate in [base, stem] {
            if !candidate.is_empty() && wanted.contains(&candidate.to_lowercase()) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn matches_like_the_windows_file_stem_ignoring_case() {
        let owner = list(&["Marathon", "RSI Launcher"]);
        assert_eq!(
            owner_match(["/opt/games/marathon"], &owner).as_deref(),
            Some("marathon")
        );
        assert_eq!(
            owner_match(["/home/me/Game/Marathon.x86_64"], &owner).as_deref(),
            Some("Marathon")
        );
        // Proton: the kernel name is truncated to 15 bytes, the first
        // argument carries the Windows path.
        assert_eq!(
            owner_match(
                [
                    "RSI Launcher.ex",
                    r"C:\Program Files\Roberts Space Industries\RSI Launcher\RSI Launcher.exe"
                ],
                &owner
            )
            .as_deref(),
            Some("RSI Launcher")
        );
    }

    #[test]
    fn substrings_and_empty_names_do_not_match() {
        let owner = list(&["steam"]);
        assert_eq!(owner_match(["steamwebhelper", "", "/"], &owner), None);
        assert_eq!(owner_match([".steam"], &owner), None);
        assert_eq!(owner_match(["steam"], &[]), None);
    }
}
