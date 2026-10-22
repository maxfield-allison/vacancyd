//! Media detection on Windows, via **power requests**.
//!
//! # Why this signal
//!
//! Idle time alone cannot tell "walked away from an unlocked desktop" from
//! "watching a two-hour film". Both look identical: no keyboard, no mouse,
//! session active. Releasing the machine on idle alone would therefore hand
//! the GPU to the cluster in the middle of a movie — the one regression that
//! `idle_lends_after_s` introduces.
//!
//! Windows already solves this problem for itself. A media player asserts a
//! **display-required power request** — `SetThreadExecutionState` with
//! `ES_DISPLAY_REQUIRED`, or `PowerSetRequest(PowerRequestDisplayRequired)` —
//! which is why the screen does not blank during playback. That request *is*
//! the statement "a human is watching this", made by the application itself,
//! and it is the same mechanism whether the player is mpv, VLC, Plex, the
//! Netflix app, or a browser tab.
//!
//! So vacancyd reads it rather than inventing a heuristic of its own. There
//! is no process allowlist to maintain, no window titles to poll, and it
//! works for players that did not exist when this was written.
//!
//! # Why `powercfg /requests`, read by structure
//!
//! There is no documented API to *read* power requests. The options were:
//!
//! - `CallNtPowerInformation(PowerRequestList)` (information level 45), the
//!   kernel call behind `powercfg`. Its result is a versioned structure that
//!   Microsoft has never published: the per-type count array grew from three
//!   to six entries across Windows 7, 8, 8.1 and 10, which moves every field
//!   after it. Reading it means guessing the version from the build number,
//!   and a wrong guess reads another field as the display count.
//! - The audio-session API (`IAudioSessionManager2`) answers a different
//!   question. It reports sound, not watching: it misses muted and silent
//!   video, and it reports music and voice calls as playback. It also lists
//!   only the calling terminal session's audio, so the session-0 service
//!   would need the per-session reporter to relay it.
//! - `powercfg.exe /requests`, the supported tool over the same kernel data.
//!   It ships in every Windows install. Some of its words are translated
//!   (German Windows 11 24H2 prints `Keine.` for `None.` and `AUSFÜHRUNG:`
//!   for `EXECUTION:`, in the console code page, while keeping `DISPLAY:`
//!   and `[PROCESS]`), and which ones is a per-language, per-build choice
//!   vacancyd cannot enumerate. Its **structure** is not translated.
//!
//! vacancyd uses `powercfg` and never reads its words. The output is one
//! section per power-request type, in the order of the `POWER_REQUEST_TYPE`
//! enumeration, so the display section is always the first. Each section is
//! a header line ending in a colon after a blank line, followed by either
//! one untagged line (the translated "None.") or holders, each tagged with
//! a bracketed kind such as `[PROCESS]`, and optional reason lines. Output
//! that does not have that shape is refused as unreadable, never read as
//! idle: see [`parse_display`].
//!
//! Reading requests **requires elevation**. A LocalSystem service has it.
//! Elevation is read from the process token before running the tool, so a
//! session-scoped, unelevated run reports [`ReadError::NeedsElevation`]
//! without depending on the language of `powercfg`'s refusal. The caller
//! turns either error into `MediaState::Unknown`, never into "nothing is
//! playing".
//!
//! # Sections
//!
//! `DISPLAY` is the section that means playback. `SYSTEM` deliberately is
//! not: on the reference machine it is held permanently by the audio
//! interface's driver ("An audio stream is currently in use"), so treating it
//! as presence would veto lending forever.

#[cfg(windows)]
use std::process::Command;
#[cfg(windows)]
use std::time::Duration;

// Constructed only by the Windows reader; the parser is tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// This process is not elevated; reading power requests is an admin
    /// operation.
    NeedsElevation,
    /// The tool could not be run, or its output was not in a readable shape.
    Failed(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsElevation => write!(
                f,
                "reading power requests requires elevation, and this process is not elevated"
            ),
            Self::Failed(e) => write!(f, "powercfg /requests: {e}"),
        }
    }
}

/// Every Windows release vacancyd supports prints these six sections, in
/// this order: the order of `POWER_REQUEST_TYPE`. Only their positions are
/// relied on. The English names are used for one cross-check: an English
/// header that appears at another position means the order changed, and the
/// output is refused.
const ENGLISH_SECTIONS: [&str; 6] = [
    "DISPLAY",
    "SYSTEM",
    "AWAYMODE",
    "EXECUTION",
    "PERFBOOST",
    "ACTIVELOCKSCREEN",
];

/// Names holding a display-required request, or why they could not be read.
#[cfg(windows)]
pub fn display_holders() -> Result<Vec<String>, ReadError> {
    if !crate::elevation::current_process_is_elevated()
        .map_err(|e| ReadError::Failed(format!("reading process elevation: {e}")))?
    {
        return Err(ReadError::NeedsElevation);
    }
    let program = crate::child_process::system_program("powercfg.exe")
        .map_err(|e| ReadError::Failed(e.to_string()))?;
    let mut command = Command::new(program);
    command.arg("/requests");
    let out = crate::child_process::output(&mut command, Duration::from_secs(5))
        .map_err(|e| ReadError::Failed(e.to_string()))?;
    if !out.status.success() {
        return Err(ReadError::Failed(format!("exited with {}", out.status)));
    }
    // The console code page encodes the translated words. Decoding them
    // lossily cannot disturb the structure, which is ASCII punctuation, and
    // holder names are shown, never matched against anything but
    // `media_ignore`.
    parse_display(&String::from_utf8_lossy(&out.stdout)).map_err(ReadError::Failed)
}

/// One section of `powercfg /requests` output: its header, without the
/// colon, and its non-empty lines, trimmed.
#[derive(Debug, PartialEq, Eq)]
struct Section<'a> {
    header: &'a str,
    body: Vec<&'a str>,
}

/// The header name on `line`, if `line` has the shape of a header: text
/// ending in a colon (ASCII or full-width), not a bracketed holder line.
/// Only meaningful for a line at the start of the output or after a blank
/// line; a reason line elsewhere may end in a colon too.
fn header_name(line: &str) -> Option<&str> {
    if line.starts_with('[') {
        return None;
    }
    let head = line
        .strip_suffix(':')
        .or_else(|| line.strip_suffix('：'))?
        .trim_end();
    (!head.is_empty()).then_some(head)
}

/// A holder line: `[KIND] name`, where the kind is translated. Returns the
/// name part.
fn holder(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let (kind, name) = rest.split_once(']')?;
    (!kind.trim().is_empty()).then(|| name.trim())
}

/// Split output into sections by structure alone.
fn sections(text: &str) -> Result<Vec<Section<'_>>, String> {
    let mut sections: Vec<Section<'_>> = Vec::new();
    let mut after_blank = true;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            after_blank = true;
            continue;
        }
        match header_name(line).filter(|_| after_blank) {
            Some(header) => sections.push(Section {
                header,
                body: Vec::new(),
            }),
            None => match sections.last_mut() {
                Some(section) => section.body.push(line),
                None => return Err("output does not begin with a section header".into()),
            },
        }
        after_blank = false;
    }
    Ok(sections)
}

/// The structural rule `display_holders` applies, kept pure so the English,
/// translated and malformed cases are tested against captured text instead
/// of a live `powercfg.exe`.
///
/// Refuses output that has fewer sections than every supported Windows
/// prints, that names an English section at the wrong position, or that has
/// a section whose body is neither holders nor the single "None." line. An
/// empty result therefore always means an empty display section.
fn parse_display(text: &str) -> Result<Vec<String>, String> {
    let sections = sections(text)?;
    if sections.len() < ENGLISH_SECTIONS.len() {
        return Err(format!(
            "expected at least {} sections, found {} (unrecognized format)",
            ENGLISH_SECTIONS.len(),
            sections.len()
        ));
    }
    for (index, section) in sections.iter().enumerate() {
        if let Some(expected) = ENGLISH_SECTIONS
            .iter()
            .position(|name| *name == section.header)
        {
            if expected != index {
                return Err(format!(
                    "section {} appears at position {} instead of {} (unrecognized format)",
                    section.header,
                    index + 1,
                    expected + 1
                ));
            }
        }
        match section.body.first() {
            None => {
                return Err(format!(
                    "section {} at position {} is empty (unrecognized format)",
                    section.header,
                    index + 1
                ))
            }
            Some(first) if holder(first).is_none() && section.body.len() != 1 => {
                return Err(format!(
                    "section {} at position {} has {} lines and no holder (unrecognized format)",
                    section.header,
                    index + 1,
                    section.body.len()
                ))
            }
            Some(_) => {}
        }
    }
    Ok(sections[0]
        .body
        .iter()
        .filter_map(|line| holder(line))
        .map(tidy)
        .collect())
}

/// Turn a raw holder string into something worth showing a human.
///
/// Processes appear as NT device paths
/// (`\Device\HarddiskVolume8\...\vlc.exe`); services append the service name
/// in parentheses; drivers are already a plain name that may itself contain
/// backslashes, so only genuine paths are shortened.
fn tidy(raw: &str) -> String {
    let (path, suffix) = match raw.split_once(" (") {
        Some((p, s)) => (p.trim(), Some(s.trim_end_matches(')'))),
        None => (raw, None),
    };
    let looks_like_a_path = path.starts_with('\\')
        || path.starts_with('/')
        || path.as_bytes().get(1).is_some_and(|c| *c == b':');
    let base = if looks_like_a_path {
        let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
        file.strip_suffix(".exe")
            .or_else(|| file.strip_suffix(".EXE"))
            .unwrap_or(file)
    } else {
        path
    };
    match suffix {
        Some(s) if !s.is_empty() => format!("{base} ({s})"),
        _ => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from the reference machine while a process held
    /// `ES_DISPLAY_REQUIRED`. If the format ever changes this fails loudly,
    /// rather than silently reporting "nothing is playing".
    const REAL: &str = r"DISPLAY:
[PROCESS] \Device\HarddiskVolume8\Windows\System32\WindowsPowerShell\v1.0\powershell.exe

SYSTEM:
[DRIVER] Focusrite USB Audio (FocusriteUSB\Audio&Adapter\1&2c3f869&0&)
An audio stream is currently in use.

AWAYMODE:
None.

EXECUTION:
None.

PERFBOOST:
None.

ACTIVELOCKSCREEN:
None.
";

    const IDLE: &str = "DISPLAY:\nNone.\n\nSYSTEM:\nNone.\n\nAWAYMODE:\nNone.\n\nEXECUTION:\nNone.\n\nPERFBOOST:\nNone.\n\nACTIVELOCKSCREEN:\nNone.\n\n";

    /// Captured from German Windows 11 Home 24H2 (build 26100) as
    /// LocalSystem, decoded the way `display_holders` decodes it: the
    /// code-page-850 `Ü` in `AUSFÜHRUNG` is not UTF-8 and becomes U+FFFD.
    const GERMAN_IDLE: &str = "DISPLAY:\r\nKeine.\r\n\r\nSYSTEM:\r\nKeine.\r\n\r\nAWAYMODE:\r\nKeine.\r\n\r\nAUSF\u{fffd}HRUNG:\r\nKeine.\r\n\r\nPERFBOOST:\r\nKeine.\r\n\r\nACTIVELOCKSCREEN:\r\nKeine.\r\n\r\n";

    /// The same German machine while a process in the signed-in session
    /// held a display request with a reason string.
    const GERMAN_PLAYING: &str = "DISPLAY:\r\n[PROCESS] \\Device\\HarddiskVolume3\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\r\nvacancyd acceptance playback\r\n\r\nSYSTEM:\r\nKeine.\r\n\r\nAWAYMODE:\r\nKeine.\r\n\r\nAUSF\u{fffd}HRUNG:\r\nKeine.\r\n\r\nPERFBOOST:\r\nKeine.\r\n\r\nACTIVELOCKSCREEN:\r\nKeine.\r\n\r\n";

    /// Captured from English Windows 11 Home 24H2 as LocalSystem: several
    /// holders under SYSTEM, and holders with reasons elsewhere.
    const ENGLISH_BUSY: &str = "DISPLAY:\r\nNone.\r\n\r\nSYSTEM:\r\n[SERVICE] \\Device\\HarddiskVolume3\\Windows\\System32\\svchost.exe (Schedule)\r\nWindows will execute '\\Microsoft\\Windows\\.NET Framework\\.NET Framework NGEN v4.0.30319 64 Critical' scheduled task that requested waking the computer.\r\n[SERVICE] \\Device\\HarddiskVolume3\\Windows\\System32\\svchost.exe (Schedule)\r\nWindows will execute '\\Microsoft\\Windows\\.NET Framework\\.NET Framework NGEN v4.0.30319 Critical' scheduled task that requested waking the computer.\r\n\r\nAWAYMODE:\r\nNone.\r\n\r\nEXECUTION:\r\n[PROCESS] \\Device\\HarddiskVolume3\\Windows\\UUS\\amd64\\MoUsoCoreWorker.exe\r\nUSO Worker\r\n\r\nPERFBOOST:\r\n[DRIVER] Legacy Kernel Caller\r\nPower Manager\r\n\r\nACTIVELOCKSCREEN:\r\nNone.\r\n\r\n";

    /// Not a capture: a language that also translates the display header
    /// and the holder kinds, which no captured build has shown but nothing
    /// rules out. Position and brackets still identify them.
    const FULLY_TRANSLATED: &str = "ANZEIGE:\nKeine.\n\nSYSTEM:\n[TREIBER] Focusrite USB Audio (FocusriteUSB\\Audio&Adapter\\1&2c3f869&0&)\nEin Audiostream wird zurzeit verwendet.\n\nAWAYMODE:\nKeine.\n\nAUSF\u{dc}HRUNG:\nKeine.\n\nPERFBOOST:\nKeine.\n\nACTIVELOCKSCREEN:\nKeine.\n\n";

    fn with_display(body: &str) -> String {
        IDLE.replacen("DISPLAY:\nNone.\n", &format!("DISPLAY:\n{body}\n"), 1)
    }

    #[test]
    fn finds_a_display_requester() {
        assert_eq!(parse_display(REAL).unwrap(), vec!["powershell"]);
    }

    #[test]
    fn the_permanent_system_holder_is_not_playback() {
        // The audio driver holds SYSTEM permanently on the reference
        // machine. If it leaked into DISPLAY the daemon would never lend.
        let text = REAL.replacen(
            "DISPLAY:\n[PROCESS] \\Device\\HarddiskVolume8\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\n",
            "DISPLAY:\nNone.\n",
            1,
        );
        assert_eq!(parse_display(&text).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn an_idle_machine_reports_nothing() {
        assert_eq!(parse_display(IDLE).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn descriptions_are_not_mistaken_for_holders() {
        let text = with_display(
            "[PROCESS] \\Device\\HarddiskVolume3\\Program Files\\VideoLAN\\VLC\\vlc.exe\nVideo playback in progress.",
        );
        assert_eq!(parse_display(&text).unwrap(), vec!["vlc"]);
    }

    #[test]
    fn a_reason_ending_in_a_colon_is_not_a_header() {
        let text = with_display("[PROCESS] C:\\a\\mpv.exe\nReason:\n[PROCESS] C:\\b\\vlc.exe");
        assert_eq!(parse_display(&text).unwrap(), vec!["mpv", "vlc"]);
    }

    #[test]
    fn services_keep_their_service_name() {
        let text = with_display(
            "[SERVICE] \\Device\\HarddiskVolume2\\Windows\\System32\\svchost.exe (WUAUSERV)",
        );
        assert_eq!(parse_display(&text).unwrap(), vec!["svchost (WUAUSERV)"]);
    }

    #[test]
    fn multiple_holders_are_all_reported() {
        let text = with_display(
            "[PROCESS] \\Device\\HarddiskVolume8\\a\\mpv.exe\n[PROCESS] C:\\Program Files\\Plex\\Plex.exe",
        );
        assert_eq!(parse_display(&text).unwrap(), vec!["mpv", "Plex"]);
    }

    #[test]
    fn captured_german_output_is_read_in_both_directions() {
        // Captured on a real German Windows: "Keine." is the empty
        // marker and a translated header sits among English ones.
        assert_eq!(parse_display(GERMAN_IDLE).unwrap(), Vec::<String>::new());
        assert_eq!(parse_display(GERMAN_PLAYING).unwrap(), vec!["powershell"]);
    }

    #[test]
    fn captured_english_output_with_other_holders_is_idle() {
        assert_eq!(parse_display(ENGLISH_BUSY).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_translated_display_header_is_read_by_position() {
        assert_eq!(
            parse_display(FULLY_TRANSLATED).unwrap(),
            Vec::<String>::new()
        );
        let playing = FULLY_TRANSLATED.replacen(
            "ANZEIGE:\nKeine.\n",
            "ANZEIGE:\n[PROZESS] \\Device\\HarddiskVolume3\\Program Files\\VideoLAN\\VLC\\vlc.exe\nVideowiedergabe\n",
            1,
        );
        assert_eq!(parse_display(&playing).unwrap(), vec!["vlc"]);
    }

    #[test]
    fn crlf_and_full_width_colons_are_structure_too() {
        let lf = GERMAN_PLAYING.replace("\r\n", "\n");
        assert_eq!(parse_display(&lf).unwrap(), vec!["powershell"]);
        let cjk = IDLE.replace("DISPLAY:", "显示：").replace("None.", "无。");
        assert_eq!(parse_display(&cjk).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn an_unrecognized_blob_refuses_rather_than_reporting_idle() {
        let garbage = "Some unexpected diagnostic text with no section headers\nat all, the kind a future powercfg change or a garbled capture could produce.\n";
        assert!(parse_display(garbage).is_err());
        // A translated refusal or error message is one untagged line with
        // no header: also refused, never read as an empty display section.
        assert!(parse_display("Zugriff verweigert.\n").is_err());
    }

    #[test]
    fn empty_output_refuses_rather_than_reporting_idle() {
        assert!(parse_display("").is_err());
        assert!(parse_display("\r\n\r\n").is_err());
    }

    #[test]
    fn truncated_output_refuses() {
        // A run cut short after the display section would otherwise look
        // like a complete idle reading.
        assert!(parse_display("DISPLAY:\nNone.\n\nSYSTEM:\nNone.\n").is_err());
        let empty_display = IDLE.replacen("DISPLAY:\nNone.\n", "DISPLAY:\n", 1);
        assert!(parse_display(&empty_display).is_err());
    }

    #[test]
    fn a_reordered_english_output_refuses() {
        let swapped = IDLE
            .replacen("DISPLAY:", "@@", 1)
            .replacen("SYSTEM:", "DISPLAY:", 1)
            .replacen("@@", "SYSTEM:", 1);
        assert!(parse_display(&swapped).is_err());
    }

    #[test]
    fn a_section_of_several_untagged_lines_refuses() {
        // Without a holder, a section must be exactly the "None." marker.
        let text = with_display("Something\nunexpected");
        assert!(parse_display(&text).is_err());
    }

    #[test]
    fn an_added_trailing_section_is_accepted() {
        let text = format!("{IDLE}NEWTYPE:\nNone.\n");
        assert_eq!(parse_display(&text).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn text_before_the_first_header_refuses() {
        // Not a header (no trailing colon), and before every section.
        assert!(parse_display(&format!("Warning: something\n\n{IDLE}")).is_err());
        // Header-shaped, so it becomes a first section that pushes DISPLAY
        // to the second position, which the English cross-check refuses.
        assert!(parse_display(&format!("Warning:\nsomething\n\n{IDLE}")).is_err());
    }

    /// The live reading on this machine, in whatever language it runs:
    /// readable when elevated, and a named elevation refusal otherwise,
    /// never an unrecognized format. Set `VACANCYD_EXPECT_DISPLAY_HOLDER`
    /// to a holder name to also require it to be seen (acceptance runs
    /// hold a display request on purpose).
    #[cfg(windows)]
    #[test]
    fn the_live_reading_is_readable_or_names_elevation() {
        let elevated = crate::elevation::current_process_is_elevated().unwrap();
        match display_holders() {
            Ok(holders) => {
                assert!(elevated);
                eprintln!("display holders: {holders:?}");
                if let Ok(expected) = std::env::var("VACANCYD_EXPECT_DISPLAY_HOLDER") {
                    assert!(
                        holders.iter().any(|h| h.eq_ignore_ascii_case(&expected)),
                        "{expected} missing from {holders:?}"
                    );
                }
            }
            Err(ReadError::NeedsElevation) => assert!(!elevated),
            Err(error) => panic!("elevated={elevated}: {error}"),
        }
    }

    #[test]
    fn errors_name_the_problem() {
        assert!(ReadError::NeedsElevation.to_string().contains("elevation"));
        let error = parse_display("DISPLAY:\nNone.\n").unwrap_err();
        assert!(error.contains("expected at least 6 sections"), "{error}");
    }
}
