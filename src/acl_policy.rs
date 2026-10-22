//! What "private to Administrators and LocalSystem" means for a Windows
//! security descriptor, judged by its effective grants rather than its text.
//!
//! The descriptor arrives as SDDL from `windows_acl`. Comparing that text
//! exactly rejected equally private descriptors: a file an elevated editor
//! saved by replace-and-rename inherits `D:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)`,
//! and an extra Administrators entry changes the string without granting
//! anyone new. This module parses the owner and DACL and asks the question
//! that matters: can anyone other than LocalSystem and Administrators gain
//! access? Kept platform-neutral so every CI runner tests it.

/// LocalSystem, Administrators, and OWNER RIGHTS (which grants only to the
/// owner, itself required to be one of the first two).
const TRUSTED_TRUSTEES: &[&str] = &["SY", "S-1-5-18", "BA", "S-1-5-32-544", "OW", "S-1-3-4"];
const TRUSTED_OWNERS: &[&str] = &["SY", "S-1-5-18", "BA", "S-1-5-32-544"];

/// ACE types that grant access. Deny types only remove access and are safe.
const ALLOW_TYPES: &[&str] = &["A", "OA", "XA", "ZA"];
const DENY_TYPES: &[&str] = &["D", "OD", "XD"];

/// `Ok` when only LocalSystem and Administrators can gain access to the
/// object: the owner is one of them (an owner can always rewrite the DACL),
/// the DACL is present, and every access-granting entry, explicit or
/// inherited, names one of them. Anything unrecognised is refused.
pub fn private_to_administrators(sddl: &str) -> Result<(), String> {
    owned_by_administrators(sddl)?;
    for ace in dacl_entries(sddl)? {
        check_ace(ace)?;
    }
    Ok(())
}

/// Accounts that may hold every right on a directory above a protected path:
/// LocalSystem, Administrators, OWNER RIGHTS (the owner, itself required to
/// be trusted), CREATOR OWNER (whoever creates the child, here the service)
/// and TrustedInstaller, which owns the Windows directory.
const REPLACING_TRUSTEES: &[&str] = &[
    "SY",
    "S-1-5-18",
    "BA",
    "S-1-5-32-544",
    "OW",
    "S-1-3-4",
    "CO",
    "S-1-3-0",
    TRUSTED_INSTALLER,
];
const TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Rights that let an account delete, rename or replace a directory or its
/// entries, or give itself those rights: DELETE, FILE_DELETE_CHILD,
/// WRITE_DAC, WRITE_OWNER and GENERIC_ALL. Creating new names is not among
/// them: ProgramData lets every user do that, and a name that is already
/// taken makes the service's own creation fail rather than follow it.
const REPLACING_RIGHTS: u32 = 0x0001_0000 | 0x0000_0040 | 0x0004_0000 | 0x0008_0000 | 0x1000_0000;

/// Rights that let an account turn an empty directory into a junction, which
/// redirects every later lookup through it: FILE_WRITE_DATA, FILE_WRITE_EA,
/// FILE_WRITE_ATTRIBUTES, GENERIC_WRITE and GENERIC_ALL. Setting a mount
/// point needs only write-data or write-attributes access to the directory.
/// A directory that still holds the next part of the path cannot be emptied
/// without the delete rights already refused, so this matters only for the
/// deepest existing one, which the service is about to create inside.
const CONVERTING_RIGHTS: u32 = 0x0000_0002 | 0x0000_0010 | 0x0000_0100 | 0x4000_0000 | 0x1000_0000;

/// `Ok` when no account other than those in [`REPLACING_TRUSTEES`] can
/// delete, rename or replace this directory or its entries, so a path
/// through it cannot be redirected: the owner is trusted (an owner can
/// always rewrite the DACL) and no access-granting entry gives anyone else
/// [`REPLACING_RIGHTS`]. Entries that only new children inherit are judged
/// too when `creating_inside`, because the service is about to create one,
/// and then no other account may hold [`CONVERTING_RIGHTS`] on this
/// directory itself either.
pub fn others_cannot_replace(sddl: &str, creating_inside: bool) -> Result<(), String> {
    let owner = components(sddl)?
        .into_iter()
        .find_map(|(tag, value)| (tag == 'O').then_some(value))
        .ok_or("the descriptor has no owner")?;
    if !is_one_of(owner, TRUSTED_OWNERS) && !is_one_of(owner, &[TRUSTED_INSTALLER]) {
        return Err(format!(
            "the owner is {owner}, not Administrators, LocalSystem or TrustedInstaller"
        ));
    }
    for ace in dacl_entries(sddl)? {
        let fields: Vec<&str> = ace.splitn(7, ';').collect();
        if fields.len() < 6 {
            return Err(format!("unrecognised access entry ({ace})"));
        }
        let kind = fields[0].trim().to_ascii_uppercase();
        if DENY_TYPES.contains(&kind.as_str()) {
            continue;
        }
        if !ALLOW_TYPES.contains(&kind.as_str()) {
            return Err(format!("unrecognised access entry type {kind}"));
        }
        let flags = fields[1].trim().to_ascii_uppercase();
        let applies_here = !flags.contains("IO");
        let new_directories_inherit = flags.contains("CI");
        let judged = applies_here || (creating_inside && new_directories_inherit);
        if !judged {
            continue;
        }
        let trustee = fields[5].trim();
        if is_one_of(trustee, REPLACING_TRUSTEES) {
            continue;
        }
        let rights = access_mask(fields[2])?;
        if rights & REPLACING_RIGHTS != 0 {
            return Err(format!(
                "{trustee} may delete, rename or change the permissions of what it holds"
            ));
        }
        if creating_inside && applies_here && rights & CONVERTING_RIGHTS != 0 {
            return Err(format!(
                "{trustee} may write to it, which would let it be turned into a junction"
            ));
        }
    }
    Ok(())
}

/// An entry's rights as a mask, from hexadecimal or SDDL's two-letter codes.
/// An unrecognised code is refused rather than read as granting nothing.
fn access_mask(rights: &str) -> Result<u32, String> {
    let rights = rights.trim();
    if let Some(hex) = rights
        .strip_prefix("0x")
        .or_else(|| rights.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16)
            .map_err(|_| format!("unrecognised access rights {rights}"));
    }
    if rights.len() % 2 != 0 {
        return Err(format!("unrecognised access rights {rights}"));
    }
    let mut mask = 0;
    for code in rights.as_bytes().chunks(2) {
        let code = std::str::from_utf8(code).map_err(|_| "access rights are not text")?;
        mask |= match code.to_ascii_uppercase().as_str() {
            "GA" => 0x1000_0000,
            "GR" => 0x8000_0000,
            "GW" => 0x4000_0000,
            "GX" => 0x2000_0000,
            "RC" => 0x0002_0000,
            "SD" => 0x0001_0000,
            "WD" => 0x0004_0000,
            "WO" => 0x0008_0000,
            "CC" => 0x0000_0001,
            "DC" => 0x0000_0002,
            "LC" => 0x0000_0004,
            "SW" => 0x0000_0008,
            "RP" => 0x0000_0010,
            "WP" => 0x0000_0020,
            "DT" => 0x0000_0040,
            "LO" => 0x0000_0080,
            "CR" => 0x0000_0100,
            "FA" => 0x001F_01FF,
            "FR" => 0x0012_0089,
            "FW" => 0x0012_0116,
            "FX" => 0x0012_00A0,
            other => return Err(format!("unrecognised access right {other}")),
        };
    }
    Ok(mask)
}

/// The access entries of a descriptor's DACL, after refusing a missing or
/// null DACL. The group grants nothing through the DACL model, and the SACL
/// governs auditing, not access.
fn dacl_entries(sddl: &str) -> Result<Vec<&str>, String> {
    let dacl = components(sddl)?
        .into_iter()
        .find_map(|(tag, value)| (tag == 'D').then_some(value))
        .ok_or("the descriptor has no DACL, which grants everyone access")?;
    let aces_start = dacl.find('(').unwrap_or(dacl.len());
    let flags = &dacl[..aces_start];
    if flags.contains("NO_ACCESS_CONTROL") {
        return Err("the DACL is null, which grants everyone access".into());
    }
    let mut entries = Vec::new();
    let mut rest = &dacl[aces_start..];
    while !rest.is_empty() {
        let body = rest
            .strip_prefix('(')
            .ok_or("the DACL has text outside an entry")?;
        let end = closing_paren(body).ok_or("an access entry is not closed")?;
        entries.push(&body[..end]);
        rest = &body[end + 1..];
    }
    Ok(entries)
}

/// `Ok` when the owner is Administrators or LocalSystem. An installer uses
/// this before adopting a file it did not write: an ordinary owner could have
/// planted it, and could still hold a handle opened before the ACL change.
pub fn owned_by_administrators(sddl: &str) -> Result<(), String> {
    let owner = components(sddl)?
        .into_iter()
        .find_map(|(tag, value)| (tag == 'O').then_some(value))
        .ok_or("the descriptor has no owner")?;
    if is_one_of(owner, TRUSTED_OWNERS) {
        Ok(())
    } else {
        Err(format!(
            "the owner is {owner}, not Administrators or LocalSystem"
        ))
    }
}

fn check_ace(ace: &str) -> Result<(), String> {
    let fields: Vec<&str> = ace.splitn(7, ';').collect();
    if fields.len() < 6 {
        return Err(format!("unrecognised access entry ({ace})"));
    }
    let kind = fields[0].trim().to_ascii_uppercase();
    let trustee = fields[5].trim();
    if DENY_TYPES.contains(&kind.as_str()) {
        return Ok(());
    }
    if !ALLOW_TYPES.contains(&kind.as_str()) {
        return Err(format!("unrecognised access entry type {kind}"));
    }
    if !is_one_of(trustee, TRUSTED_TRUSTEES) {
        return Err(format!("access is granted to {trustee}"));
    }
    Ok(())
}

fn is_one_of(sid: &str, allowed: &[&str]) -> bool {
    allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(sid.trim()))
}

/// Split a descriptor into its `O:`, `G:`, `D:` and `S:` parts. A tag only
/// counts outside an access entry, whose conditions may contain colons.
fn components(sddl: &str) -> Result<Vec<(char, &str)>, String> {
    let bytes = sddl.as_bytes();
    let mut starts = Vec::new();
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.checked_sub(1).ok_or("unbalanced descriptor")?,
            b'O' | b'G' | b'D' | b'S' if depth == 0 && bytes.get(index + 1) == Some(&b':') => {
                starts.push(index)
            }
            _ => {}
        }
    }
    if depth != 0 || starts.first() != Some(&0) {
        return Err("the descriptor is not in a recognised form".into());
    }
    let mut parts = Vec::new();
    let mut seen = String::new();
    for (position, start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(sddl.len());
        let tag = bytes[*start] as char;
        if seen.contains(tag) {
            return Err(format!("the descriptor repeats its {tag}: part"));
        }
        seen.push(tag);
        parts.push((tag, &sddl[start + 2..end]));
    }
    Ok(parts)
}

fn closing_paren(body: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, ch) in body.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(index),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::private_to_administrators as check;

    #[test]
    fn descriptors_written_by_vacancyd_and_the_msi_are_private() {
        for sddl in [
            "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)",
            "O:BAD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
            "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
            "O:BAG:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)",
        ] {
            assert_eq!(check(sddl), Ok(()), "{sddl}");
        }
    }

    #[test]
    fn equally_private_descriptors_are_accepted() {
        for sddl in [
            // An elevated editor's replace-and-rename save inherits from the
            // protected directory instead of carrying a protected DACL.
            "O:BAD:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)",
            // A second Administrators entry or a narrower grant.
            "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BA)",
            // SYSTEM as owner, full SID spellings, lowercase.
            "O:S-1-5-18D:P(A;;FA;;;S-1-5-18)(A;;FA;;;s-1-5-32-544)",
            // Deny entries only remove access.
            "O:BAD:P(D;;FA;;;WD)(A;;FA;;;SY)(A;;FA;;;BA)",
            // OWNER RIGHTS grants only to the (trusted) owner.
            "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x60000;;;OW)",
            // An empty DACL grants nobody.
            "O:BAD:P",
        ] {
            assert_eq!(check(sddl), Ok(()), "{sddl}");
        }
    }

    #[test]
    fn any_other_principal_or_owner_is_refused() {
        for sddl in [
            "O:BAD:AI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;0x1200a9;;;BU)",
            "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;AU)",
            "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1-2-3-1001)",
            "O:BAD:P(A;OICIIO;GA;;;CO)(A;;FA;;;SY)",
            "O:BAD:P(XA;;FA;;;WD;(Member_of {SID(BA)}))",
            "O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;SY)(A;;FA;;;BA)",
            "O:BUD:P(A;;FA;;;SY)",
        ] {
            assert!(check(sddl).is_err(), "{sddl}");
        }
    }

    #[test]
    fn installer_adopts_only_files_owned_by_administrators_or_system() {
        use super::owned_by_administrators as owned;
        assert_eq!(owned("O:BAD:AI(A;ID;FA;;;BU)"), Ok(()));
        assert_eq!(owned("O:SYG:SYD:P"), Ok(()));
        assert_eq!(owned("O:S-1-5-32-544D:P"), Ok(()));
        assert!(owned("O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;SY)(A;;FA;;;BA)").is_err());
        assert!(owned("O:BUD:P").is_err());
        assert!(owned("D:P(A;;FA;;;SY)").is_err());
        assert!(owned("").is_err());
    }

    // Descriptors in the shape of Windows' defaults for the drive root,
    // ProgramData, the Windows directory and a user's profile. The native
    // test in `secure_file` reads the real ones on a Windows machine.
    const DRIVE_ROOT: &str = "O:SYG:SYD:PAI(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICIIO;GA;;;CO)(A;OICI;0x1200a9;;;BU)(A;CI;LC;;;AU)(A;CIIO;SDGXGWGR;;;AU)";
    const PROGRAM_DATA: &str = "O:SYG:SYD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICIIO;GA;;;CO)(A;OICI;0x1200a9;;;BU)(A;CI;DCLCRPCR;;;BU)";
    const WINDOWS: &str = "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464G:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464D:PAI(A;;FA;;;S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)(A;CIIO;GA;;;S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464)(A;;0x1301bf;;;SY)(A;OICIIO;GA;;;SY)(A;;0x1301bf;;;BA)(A;OICIIO;GA;;;BA)(A;;0x1200a9;;;BU)(A;OICIIO;GXGR;;;BU)(A;OICIIO;GA;;;CO)";
    const USER_PROFILE: &str = "O:S-1-5-21-1-2-3-1001G:S-1-5-21-1-2-3-513D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;S-1-5-21-1-2-3-1001)";

    #[test]
    fn system_directories_cannot_be_redirected_by_other_accounts() {
        let check = |sddl: &str, inside: bool| super::others_cannot_replace(sddl, inside);
        for sddl in [DRIVE_ROOT, PROGRAM_DATA, WINDOWS] {
            assert_eq!(check(sddl, false), Ok(()), "{sddl}");
        }
    }

    #[test]
    fn the_deepest_existing_directory_must_not_be_writable_by_other_accounts() {
        let check = |sddl: &str, inside: bool| super::others_cannot_replace(sddl, inside);
        // Write access to an empty directory is enough to make it a junction
        // after the check, redirecting the directory created inside it.
        let writable = "O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FW;;;BU)";
        assert!(check(writable, true)
            .unwrap_err()
            .contains("turned into a junction"));
        for entry in [
            "(A;;GW;;;BU)",
            "(A;;DC;;;BU)",
            "(A;;CR;;;BU)",
            "(A;;RP;;;BU)",
            "(A;ID;0x2;;;AU)",
        ] {
            let sddl = format!("O:SYD:P(A;OICI;FA;;;SY){entry}");
            assert!(check(&sddl, true).is_err(), "{sddl}");
        }
        // Higher up the path a directory still holds the next part, which
        // only the delete rights refused above could remove.
        assert_eq!(check(writable, false), Ok(()));
        // ProgramData gives Users write access to itself, so the service
        // creates nothing directly in it, but a path through it is accepted.
        assert!(check(PROGRAM_DATA, true).is_err());
        assert_eq!(check(PROGRAM_DATA, false), Ok(()));
        // Write rights that only new children would inherit are accepted
        // because the service never lets its directories inherit: each is
        // created with its protected descriptor in the same call (see
        // `windows_acl::create_protected_directory` and its native test).
        assert_eq!(
            check("O:SYD:P(A;OICI;FA;;;SY)(A;CIIO;FW;;;BU)", true),
            Ok(())
        );
    }

    #[test]
    fn a_directory_another_account_can_change_or_empty_is_refused() {
        let check = |sddl: &str, inside: bool| super::others_cannot_replace(sddl, inside);
        // A user's own folder: the user owns it and holds every right.
        // That includes an administrator's own profile: elevation keeps the
        // user's identity, so the same user's unelevated programs could
        // still redirect a path through it.
        assert!(check(USER_PROFILE, false).is_err());
        assert!(check(USER_PROFILE, true).is_err());
        for entry in [
            "(A;;FA;;;BU)",
            "(A;;0x1301bf;;;AU)",
            "(A;;DT;;;BU)",
            "(A;;SD;;;S-1-5-21-1-2-3-1001)",
            "(A;CI;WD;;;BU)",
            "(A;;WO;;;WD)",
            "(A;;GA;;;BU)",
        ] {
            let sddl = format!("O:SYD:P(A;OICI;FA;;;SY){entry}");
            let refused = check(&sddl, false).unwrap_err();
            assert!(refused.contains("may delete"), "{sddl}: {refused}");
        }
        // The drive root gives Authenticated Users DELETE on any folder
        // created directly in it, so the service creates none there.
        assert!(check(DRIVE_ROOT, true)
            .unwrap_err()
            .contains("AU may delete"));
    }

    #[test]
    fn an_unreadable_right_or_owner_is_refused() {
        let check = |sddl: &str, inside: bool| super::others_cannot_replace(sddl, inside);
        assert!(check("O:SYD:P(A;;ZZ;;;BU)", false).is_err());
        assert!(check("O:SYD:P(A;;0xnothex;;;BU)", false).is_err());
        assert!(check("O:BUD:P(A;;FA;;;SY)", false).is_err());
        assert!(check("O:SYD:NO_ACCESS_CONTROL", false).is_err());
        assert!(check("O:SY", false).is_err());
        // Read access for everyone does not let anyone redirect a path.
        assert_eq!(
            check("O:SYD:P(A;;FR;;;WD)(A;;0x1200a9;;;BU)", false),
            Ok(())
        );
    }

    #[test]
    fn missing_null_or_malformed_descriptors_are_refused() {
        for sddl in [
            "",
            "D:P(A;;FA;;;SY)",
            "O:BA",
            "O:BAD:NO_ACCESS_CONTROL",
            "O:BAD:P(A;;FA;;;SY",
            "O:BAD:P(A;;FA;;;SY)junk",
            "O:BAD:P(AU;SA;FA;;;WD)",
            "O:BAD:P(A;;FA)",
            "O:BAO:SYD:P",
            "junkO:BAD:P",
        ] {
            assert!(check(sddl).is_err(), "{sddl:?}");
        }
    }
}
