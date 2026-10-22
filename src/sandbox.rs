//! Which configured paths the systemd sandbox would stop the service writing.
//!
//! The packaged unit runs with `ProtectSystem=strict`: the whole file system
//! is read-only to the service except the directories it names. A path
//! outside them validates, and then fails only at runtime, in a different
//! way for each setting. `check-config` runs outside that sandbox, so it
//! cannot find out by trying; it reads the unit's settings instead and warns
//! about each path they leave read-only.
//!
//! The settings come from the installed `vacancyd.service` when systemd has
//! one loaded (including any drop-in), and from the packaged unit otherwise,
//! which is the case when `check-config` runs before the unit is installed,
//! as the setup procedure does. A host not booted with systemd, such as a
//! container, has no sandbox to compare against and gets no warnings.
//!
//! Paths are compared as written, after resolving a relative one against the
//! configuration's directory. A symbolic link into a writable directory is
//! still reported, because systemd applies these rules to the paths it
//! resolves and this check does not follow links.

use std::path::{Component, Path, PathBuf};

use crate::config::Config;

/// The unit systemd runs the service under in the packaged install.
const UNIT: &str = "vacancyd.service";
const PACKAGED_UNIT: &str = include_str!("../contrib/vacancyd.service");
/// The unit settings that decide what the service can write.
const PROPERTIES: &[&str] = &[
    "ProtectSystem",
    "ProtectHome",
    "PrivateTmp",
    "ReadWritePaths",
    "ReadOnlyPaths",
    "InaccessiblePaths",
    "StateDirectory",
    "RuntimeDirectory",
    "CacheDirectory",
    "LogsDirectory",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Writable,
    ReadOnly,
    Inaccessible,
}

/// What a unit lets the service write: the most specific rule covering a
/// path decides, as it does for systemd's own path settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// Where these settings came from, for the warning.
    source: &'static str,
    rules: Vec<(PathBuf, Access)>,
}

impl Sandbox {
    /// Read unit settings from `Key=Value` lines, the syntax of both a unit
    /// file and `systemctl show`. Comments and section headers are skipped,
    /// and a repeated list setting adds to the earlier ones.
    fn parse(source: &'static str, text: &str) -> Self {
        let mut rules = Vec::new();
        let mut rule = |path: &str, access| rules.push((PathBuf::from(path), access));
        for line in text.lines().map(str::trim) {
            if line.starts_with(['#', ';']) {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let words = value.split_whitespace();
            match key.trim() {
                "ProtectSystem" => match value {
                    "strict" => rule("/", Access::ReadOnly),
                    "full" => {
                        for path in ["/usr", "/boot", "/efi", "/etc"] {
                            rule(path, Access::ReadOnly);
                        }
                    }
                    "yes" | "true" => {
                        for path in ["/usr", "/boot", "/efi"] {
                            rule(path, Access::ReadOnly);
                        }
                    }
                    _ => {}
                },
                "ProtectHome" => {
                    let access = match value {
                        "yes" | "true" => Access::Inaccessible,
                        // A tmpfs here is mounted read-only.
                        "read-only" | "tmpfs" => Access::ReadOnly,
                        _ => continue,
                    };
                    for path in ["/home", "/root", "/run/user"] {
                        rule(path, access);
                    }
                }
                "PrivateTmp" if matches!(value, "yes" | "true") => {
                    rule("/tmp", Access::Writable);
                    rule("/var/tmp", Access::Writable);
                }
                "ReadWritePaths" => words.for_each(|w| rule(unprefixed(w), Access::Writable)),
                "ReadOnlyPaths" => words.for_each(|w| rule(unprefixed(w), Access::ReadOnly)),
                "InaccessiblePaths" => {
                    words.for_each(|w| rule(unprefixed(w), Access::Inaccessible))
                }
                key @ ("StateDirectory" | "RuntimeDirectory" | "CacheDirectory"
                | "LogsDirectory") => {
                    let base = match key {
                        "StateDirectory" => "/var/lib",
                        "RuntimeDirectory" => "/run",
                        "CacheDirectory" => "/var/cache",
                        _ => "/var/log",
                    };
                    for name in words {
                        // `name:alias` adds a symbolic link; the name is the directory.
                        let name = name.split(':').next().unwrap_or(name);
                        rule(&format!("{base}/{name}"), Access::Writable);
                    }
                }
                _ => {}
            }
        }
        Self { source, rules }
    }

    /// The packaged unit, `contrib/vacancyd.service`.
    pub fn packaged() -> Self {
        Self::parse("the packaged systemd unit", PACKAGED_UNIT)
    }

    /// The installed unit when systemd has one loaded, the packaged unit when
    /// it does not, and `None` on a host not booted with systemd.
    pub fn current() -> Option<Self> {
        if !Path::new("/run/systemd/system").is_dir() {
            return None;
        }
        let output = std::process::Command::new("systemctl")
            .arg("show")
            .arg(UNIT)
            .arg(format!("--property=LoadState,{}", PROPERTIES.join(",")))
            .output();
        let installed = output
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .filter(|text| text.lines().any(|line| line.trim() == "LoadState=loaded"));
        Some(match installed {
            Some(text) => Self::parse("the installed vacancyd.service", &text),
            None => Self::packaged(),
        })
    }

    /// Whether the service could not write `path` under these settings.
    fn blocks(&self, path: &Path) -> bool {
        let path = lexical(path);
        self.rules
            .iter()
            .filter(|(prefix, _)| path.starts_with(lexical(prefix)))
            .max_by_key(|(prefix, _)| lexical(prefix).components().count())
            .is_some_and(|(_, access)| *access != Access::Writable)
    }
}

/// `-` (ignore if missing) and `+` (outside the root directory) prefix a path
/// in these settings; neither changes which directory it names.
fn unprefixed(word: &str) -> &str {
    word.trim_start_matches(['-', '+'])
}

/// `path` without `.` components or a `..` that has something to cancel.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A directory the service must be able to write, the setting that names
/// it, and what happens at runtime when it cannot.
struct Needed {
    setting: &'static str,
    directory: PathBuf,
    consequence: &'static str,
}

fn needed(config: &Config, config_path: &Path) -> Vec<Needed> {
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(config_path))
            .unwrap_or_else(|_| config_path.to_path_buf())
    };
    let config_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut needed = vec![Needed {
        setting: "the configuration file",
        directory: config_dir.clone(),
        consequence: "saving settings (vacancyd admin-request or POST /admin/settings) fails",
    }];
    // Empty uses $STATE_DIRECTORY/admission under systemd, which the unit
    // makes writable.
    if !config.server.admission_state_dir.is_empty() {
        let path = Path::new(&config.server.admission_state_dir);
        needed.push(Needed {
            setting: "server.admission_state_dir",
            directory: config_dir.join(path),
            consequence: "the service stops at startup",
        });
    }
    if config.owner.gpu_pressure.enabled {
        let cache = config.owner.gpu_pressure.state_path(&config_path);
        needed.push(Needed {
            setting: "owner.gpu_pressure.state_file",
            directory: cache.parent().map(Path::to_path_buf).unwrap_or(config_dir),
            consequence: "GPU calibration is not saved, and a warning is logged every five minutes",
        });
    }
    let socket = config.server.admin_socket_path();
    if let Some(directory) = socket.parent() {
        needed.push(Needed {
            setting: "server.admin_socket",
            directory: directory.to_path_buf(),
            consequence: "the local admin channel stays down while the service otherwise runs; /status reports admin_channel unavailable",
        });
    }
    needed
}

/// Each configured directory `sandbox` would leave read-only, as a warning
/// naming the setting, the directory, the effect and the drop-in that fixes it.
fn blocked(config: &Config, config_path: &Path, sandbox: &Sandbox) -> Vec<String> {
    needed(config, config_path)
        .into_iter()
        .filter(|need| sandbox.blocks(&need.directory))
        .map(|need| {
            let directory = lexical(&need.directory);
            format!(
                "{} needs {} to be writable, and {} does not allow it: {}. Create the directory and allow it with a drop-in: run `sudo systemctl edit vacancyd` and add `[Service]` `ReadWritePaths={}`",
                need.setting,
                directory.display(),
                sandbox.source,
                need.consequence,
                directory.display(),
            )
        })
        .collect()
}

/// `check-config`: warn about each configured path the systemd sandbox would
/// stop the service writing. Silent on a host without systemd.
pub fn warn_blocked_paths(config: &Config, config_path: &Path) {
    let Some(sandbox) = Sandbox::current() else {
        return;
    };
    for warning in blocked(config, config_path, &sandbox) {
        tracing::warn!("{warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example, which enables GPU pressure and leaves every
    /// path at its default.
    fn config() -> Config {
        toml::from_str(include_str!("../vacancyd.example.toml")).unwrap()
    }

    #[test]
    fn the_packaged_unit_writes_only_its_own_directories() {
        let unit = Sandbox::packaged();
        for writable in [
            "/etc/vacancyd",
            "/etc/vacancyd/sub",
            "/var/lib/vacancyd",
            "/var/lib/vacancyd/admission",
            "/run/vacancyd",
            "/tmp",
        ] {
            assert!(!unit.blocks(Path::new(writable)), "{writable}");
        }
        for blocked in [
            "/etc",
            "/etc/vacancyd-other",
            "/var/lib",
            "/srv/vacancyd",
            "/run/lender",
            "/home/owner/vacancyd",
            "/var/lib/vacancyd/../other",
        ] {
            assert!(unit.blocks(Path::new(blocked)), "{blocked}");
        }
    }

    #[test]
    fn the_installed_units_settings_and_drop_ins_decide() {
        // As `systemctl show` prints them, with a drop-in adding /srv/lender.
        let shown = "LoadState=loaded\nProtectSystem=strict\nProtectHome=yes\nPrivateTmp=no\n\
                     ReadWritePaths=-/etc/vacancyd /srv/lender\nReadOnlyPaths=/srv/lender/ro\n\
                     InaccessiblePaths=\nStateDirectory=vacancyd:alias\nRuntimeDirectory=vacancyd\n";
        let unit = Sandbox::parse("test", shown);
        assert!(!unit.blocks(Path::new("/srv/lender/admission")));
        assert!(unit.blocks(Path::new("/srv/lender/ro")));
        assert!(!unit.blocks(Path::new("/var/lib/vacancyd")));
        assert!(unit.blocks(Path::new("/var/lib/alias")));
        assert!(unit.blocks(Path::new("/tmp")), "PrivateTmp=no");
        assert!(unit.blocks(Path::new("/home/owner")));

        let loose = Sandbox::parse("test", "ProtectSystem=full\nProtectHome=no\n");
        assert!(!loose.blocks(Path::new("/srv/lender")));
        assert!(loose.blocks(Path::new("/etc/vacancyd")));
        assert!(!Sandbox::parse("test", "ProtectSystem=no\n").blocks(Path::new("/etc")));
    }

    #[test]
    fn check_config_names_each_path_the_packaged_unit_would_block() {
        let mut cfg = config();
        cfg.server.admission_state_dir = "/srv/vacancyd/admission".into();
        cfg.server.admin_socket = "/srv/vacancyd/run/admin.sock".into();
        cfg.owner.gpu_pressure.enabled = true;
        cfg.owner.gpu_pressure.state_file = "/srv/vacancyd/cache/pressure.json".into();

        let warnings = blocked(
            &cfg,
            Path::new("/etc/vacancyd/vacancyd.toml"),
            &Sandbox::packaged(),
        );

        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        assert!(warnings[0].starts_with("server.admission_state_dir needs /srv/vacancyd/admission to be writable, and the packaged systemd unit does not allow it: the service stops at startup."), "{}", warnings[0]);
        assert!(
            warnings[0].ends_with("`ReadWritePaths=/srv/vacancyd/admission`"),
            "{}",
            warnings[0]
        );
        assert!(
            warnings[1].starts_with("owner.gpu_pressure.state_file needs /srv/vacancyd/cache "),
            "{}",
            warnings[1]
        );
        assert!(
            warnings[2].starts_with("server.admin_socket needs /srv/vacancyd/run "),
            "{}",
            warnings[2]
        );
    }

    #[test]
    fn the_documented_linux_paths_pass_under_the_packaged_unit() {
        let mut cfg = config();
        cfg.server.admission_state_dir = "/var/lib/vacancyd/admission".into();
        cfg.owner.gpu_pressure.state_file = "/var/lib/vacancyd/pressure.json".into();
        let path = Path::new("/etc/vacancyd/vacancyd.toml");
        assert_eq!(
            blocked(&cfg, path, &Sandbox::packaged()),
            Vec::<String>::new()
        );

        // A configuration kept elsewhere cannot be saved, and its default
        // pressure cache beside it cannot be written either.
        cfg.owner.gpu_pressure.state_file.clear();
        let elsewhere = blocked(
            &cfg,
            Path::new("/opt/vacancyd/vacancyd.toml"),
            &Sandbox::packaged(),
        );
        assert_eq!(elsewhere.len(), 2, "{elsewhere:#?}");
        assert!(elsewhere[0].starts_with("the configuration file needs /opt/vacancyd "));
        assert!(elsewhere[1].starts_with("owner.gpu_pressure.state_file needs /opt/vacancyd "));
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_configuration() {
        let mut cfg = config();
        cfg.owner.gpu_pressure.enabled = false;
        cfg.server.admission_state_dir = "../lender/admission".into();
        let warnings = blocked(
            &cfg,
            Path::new("/etc/vacancyd/vacancyd.toml"),
            &Sandbox::packaged(),
        );
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(
            warnings[0].contains("needs /etc/lender/admission "),
            "{}",
            warnings[0]
        );
    }
}
