//! Which processes are the ones vacancyd lends to?
//!
//! The GPU pressure signal has to subtract the borrower's own work, or it
//! detects itself: borrowed inference drives utilisation up, that reads as the
//! owner returning, the veto engages, the model is evicted, utilisation falls,
//! the veto releases, and lending oscillates on a `release_after_s` period.
//!
//! That set could have been another configuration list, but it does not need to
//! be. vacancyd already knows exactly what it forwards to — the `target` of
//! every upstream — so the lender process tree starts with whatever serves
//! those targets on this machine and includes every descendant. Deriving it
//! avoids a process-name list and follows service restarts automatically.
//!
//! Only a target that is demonstrably on this machine, in this network
//! namespace, can be matched against its listener table: a loopback address,
//! `localhost`, or an address of one of this machine's interfaces. A target
//! on another host, or named by any other host name (a container service
//! name, for example), is never matched by port alone, because an unrelated
//! local socket on the same port says nothing about it.
//!
//! This is an approximation, not request provenance: owner and borrower
//! requests served by the same process cannot be distinguished. A dedicated
//! borrowed-traffic instance remains the exact deployment shape.
//!
//! Nor does it follow a listener that only relays connections, for example
//! into a virtual machine: the work then runs outside the tree and counts as
//! owner load. That is reported rather than guessed at; see
//! [`crate::collect::unseen_work`].
//!
//! On Windows the listening sockets and their owning processes come from the
//! IP Helper API (`GetExtendedTcpTable` with `TCP_TABLE_OWNER_PID_LISTENER`),
//! which returns numbers rather than text, so nothing depends on the display
//! language. On Linux they come from `/proc/net/tcp` and `/proc/net/tcp6`.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use sysinfo::{ProcessesToUpdate, System};

/// A process identity that does not survive PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Process creation time in seconds since the Unix epoch, as reported by
    /// sysinfo. The pair is materially safer than retaining a bare PID.
    pub started_at_s: u64,
}

#[derive(Debug, Clone)]
struct ProcessRecord {
    identity: ProcessIdentity,
    parent_pid: Option<u32>,
    name: String,
}

/// Network exposure of an upstream's actual target listener. `Unknown` is a
/// successful observation with no listener serving a local target, and
/// `NotObservable` a target this machine's listener table cannot describe;
/// a failed listener read is represented by an observation failure in
/// runtime state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListenerState {
    LoopbackOnly,
    ListensOnNetwork,
    #[default]
    Unknown,
    NotObservable,
}

impl ListenerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoopbackOnly => "loopback_only",
            Self::ListensOnNetwork => "listens_on_network",
            Self::Unknown => "unknown",
            Self::NotObservable => "not_observable",
        }
    }
}

/// One coherent process-tree snapshot used for both descendant expansion and
/// attribution. A single snapshot prevents a PID from being classified using
/// a name or start time observed in a different process generation.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    records: std::collections::HashMap<u32, ProcessRecord>,
}

impl ProcessTable {
    pub fn capture() -> Self {
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let records = system
            .processes()
            .values()
            .map(|process| {
                let pid = process.pid().as_u32();
                (
                    pid,
                    ProcessRecord {
                        identity: ProcessIdentity {
                            pid,
                            started_at_s: process.start_time(),
                        },
                        parent_pid: process.parent().map(|pid| pid.as_u32()),
                        name: process.name().to_string_lossy().into_owned(),
                    },
                )
            })
            .collect();
        Self { records }
    }

    pub fn identity(&self, pid: u32) -> Option<ProcessIdentity> {
        self.records.get(&pid).map(|record| record.identity)
    }

    pub fn name(&self, pid: u32) -> Option<String> {
        self.records.get(&pid).map(|record| record.name.clone())
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: &[(u32, u64, Option<u32>, &str)]) -> Self {
        Self {
            records: entries
                .iter()
                .map(|(pid, started_at_s, parent_pid, name)| {
                    (
                        *pid,
                        ProcessRecord {
                            identity: ProcessIdentity {
                                pid: *pid,
                                started_at_s: *started_at_s,
                            },
                            parent_pid: *parent_pid,
                            name: (*name).to_string(),
                        },
                    )
                })
                .collect(),
        }
    }
}

/// Expand listener PIDs through every descendant present in the same process
/// snapshot, returning start-time-qualified identities.
pub fn lender_processes(
    listener_pids: &HashSet<u32>,
    table: &ProcessTable,
) -> HashSet<ProcessIdentity> {
    let mut included_pids: HashSet<u32> = listener_pids
        .iter()
        .filter(|pid| table.records.contains_key(pid))
        .copied()
        .collect();
    loop {
        let before = included_pids.len();
        for record in table.records.values() {
            if record
                .parent_pid
                .is_some_and(|parent| included_pids.contains(&parent))
            {
                included_pids.insert(record.identity.pid);
            }
        }
        if included_pids.len() == before {
            break;
        }
    }
    included_pids
        .into_iter()
        .filter_map(|pid| table.identity(pid))
        .collect()
}

/// An upstream target that is on this machine: the addresses a connection
/// to it may use, and its port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTarget {
    addresses: Vec<IpAddr>,
    port: u16,
}

impl LocalTarget {
    /// The local target `target` names, or `None` if it is not demonstrably
    /// on this machine: another host, or a host name other than `localhost`.
    /// `local_addresses` are the addresses of this machine's interfaces;
    /// they are needed only for a target given as a non-loopback address.
    pub fn parse(target: &str, local_addresses: impl FnOnce() -> HashSet<IpAddr>) -> Option<Self> {
        let url = reqwest::Url::parse(target).ok()?;
        let port = url.port_or_known_default()?;
        // The URL parser has already normalized address literals.
        let host = url.host_str()?;
        let addresses = if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            vec![IpAddr::V6(v6.parse().ok()?).to_canonical()]
        } else if let Ok(v4) = host.parse::<Ipv4Addr>() {
            vec![IpAddr::V4(v4)]
        } else if host
            .strip_suffix('.')
            .unwrap_or(host)
            .eq_ignore_ascii_case("localhost")
        {
            vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]
        } else {
            return None;
        };
        let address = addresses[0];
        if address.is_unspecified() {
            // Connecting to the unspecified address reaches this machine.
            let loopback = match address {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            };
            return Some(Self {
                addresses: vec![loopback],
                port,
            });
        }
        if !address.is_loopback() && !local_addresses().contains(&address) {
            return None;
        }
        Some(Self { addresses, port })
    }

    /// Listeners that accept connections to this target.
    fn serving<'a>(&'a self, rows: &'a [ListenerRow]) -> impl Iterator<Item = &'a ListenerRow> {
        rows.iter().filter(|row| {
            row.port == self.port
                && self
                    .addresses
                    .iter()
                    .any(|address| accepts(row.local, *address))
        })
    }

    /// The exposure of the listener serving this target. Other listeners on
    /// the same port count when they belong to a serving process, or, where
    /// the table does not name owners (Linux), whatever they belong to: the
    /// uncertainty errs towards reporting network exposure.
    fn exposure(&self, rows: &[ListenerRow]) -> ListenerState {
        let serving: Vec<&ListenerRow> = self.serving(rows).collect();
        if serving.is_empty() {
            return ListenerState::Unknown;
        }
        let owners: Option<HashSet<u32>> = serving.iter().map(|row| row.pid).collect();
        let network = rows.iter().any(|row| {
            row.port == self.port
                && owners
                    .as_ref()
                    .is_none_or(|owners| row.pid.is_some_and(|pid| owners.contains(&pid)))
                && !row.local.is_loopback()
        });
        if network {
            ListenerState::ListensOnNetwork
        } else {
            ListenerState::LoopbackOnly
        }
    }
}

/// Whether a socket bound to `bound` accepts a connection to `target`. An
/// IPv6 wildcard may be dual-stack, which the table does not say, so it is
/// taken to accept IPv4 connections too.
fn accepts(bound: IpAddr, target: IpAddr) -> bool {
    bound == target || (bound.is_unspecified() && (bound.is_ipv6() || target.is_ipv4()))
}

/// The addresses of this machine's network interfaces. An unreadable list is
/// empty, which makes a target given by such an address not observable,
/// never matched by guess.
fn interface_addresses() -> HashSet<IpAddr> {
    sysinfo::Networks::new_with_refreshed_list()
        .values()
        .flat_map(|data| {
            data.ip_networks()
                .iter()
                .map(|network| network.addr.to_canonical())
        })
        .collect()
}

/// The local targets among `targets`, in order, with the interface list read
/// at most once and only if a target needs it.
fn local_targets(targets: &[String]) -> Vec<Option<LocalTarget>> {
    let mut interfaces: Option<HashSet<IpAddr>> = None;
    targets
        .iter()
        .map(|target| {
            LocalTarget::parse(target, || {
                interfaces.get_or_insert_with(interface_addresses).clone()
            })
        })
        .collect()
}

/// The listener exposure of each of `targets`, in order. A target that is
/// not on this machine is `NotObservable` without reading anything; the
/// others share one read of the listener table, whose failure is reported
/// for each of them.
pub fn target_listener_states(targets: &[String]) -> Vec<Result<ListenerState, String>> {
    let locals = local_targets(targets);
    let rows = if locals.iter().any(Option::is_some) {
        listener_rows().map_err(|error| error.to_string())
    } else {
        Ok(Vec::new())
    };
    locals
        .iter()
        .map(|local| match (local, &rows) {
            (None, _) => Ok(ListenerState::NotObservable),
            (Some(local), Ok(rows)) => Ok(local.exposure(rows)),
            (Some(_), Err(error)) => Err(error.clone()),
        })
        .collect()
}

/// The processes serving each of `targets`, in order: `None` for a target
/// that is not on this machine, otherwise the listener processes accepting
/// it, which may be none. One listener table read serves every target.
///
/// An error means the listing could not be taken. A caller must not read it
/// as "no lender": the safe fallback is an empty lender set, which means
/// borrowed work is *not* subtracted, so pressure reads high and the veto
/// errs towards protecting the owner rather than towards lending a machine
/// in use.
#[cfg(windows)]
pub fn pids_serving(targets: &[String]) -> anyhow::Result<Vec<Option<HashSet<u32>>>> {
    let locals = local_targets(targets);
    let rows = if locals.iter().any(Option::is_some) {
        listener_rows()?
    } else {
        Vec::new()
    };
    Ok(locals
        .iter()
        .map(|local| local.as_ref().map(|local| serving_pids(local, &rows)))
        .collect())
}

/// GPU attribution, the only consumer, is Windows-only, so no other platform
/// resolves lender processes yet.
#[cfg(not(windows))]
pub fn pids_serving(_targets: &[String]) -> anyhow::Result<Vec<Option<HashSet<u32>>>> {
    anyhow::bail!("lender process discovery is not implemented on this platform")
}

#[cfg(any(windows, test))]
fn serving_pids(target: &LocalTarget, rows: &[ListenerRow]) -> HashSet<u32> {
    target.serving(rows).filter_map(|row| row.pid).collect()
}

/// One listening TCP socket, as the operating system reports it. Linux's
/// table does not name the owning process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListenerRow {
    local: IpAddr,
    port: u16,
    pid: Option<u32>,
}

#[cfg(windows)]
fn listener_rows() -> anyhow::Result<Vec<ListenerRow>> {
    windows_tcp::listeners()
}

/// Linux exposes bound TCP sockets without a locale-sensitive subprocess.
/// Both families are read; a missing IPv6 table means IPv6 is disabled.
#[cfg(target_os = "linux")]
fn listener_rows() -> anyhow::Result<Vec<ListenerRow>> {
    let mut rows = proc_net_rows(&std::fs::read_to_string("/proc/net/tcp")?, false);
    match std::fs::read_to_string("/proc/net/tcp6") {
        Ok(tcp6) => rows.extend(proc_net_rows(&tcp6, true)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(rows)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn listener_rows() -> anyhow::Result<Vec<ListenerRow>> {
    anyhow::bail!("listener observation is unsupported on this platform")
}

#[cfg(any(target_os = "linux", test))]
fn proc_net_rows(table: &str, ipv6: bool) -> Vec<ListenerRow> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            // sl, local_address, rem_address, st ...; 0A is TCP_LISTEN.
            if fields.len() < 4 || fields[3] != "0A" {
                return None;
            }
            let (host, port) = fields[1].split_once(':')?;
            Some(ListenerRow {
                local: proc_net_address(host, ipv6)?,
                port: u16::from_str_radix(port, 16).ok()?,
                pid: None,
            })
        })
        .collect()
}

/// A `/proc/net/tcp{,6}` address: 32-bit words in host (little-endian) order.
#[cfg(any(target_os = "linux", test))]
fn proc_net_address(host: &str, ipv6: bool) -> Option<IpAddr> {
    if !ipv6 {
        let raw = u32::from_str_radix(host, 16).ok()?;
        return Some(IpAddr::V4(Ipv4Addr::from(raw.to_le_bytes())));
    }
    if host.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    let (words, _) = host.as_bytes().as_chunks::<8>();
    for (index, chunk) in words.iter().enumerate() {
        let chunk = std::str::from_utf8(chunk).ok()?;
        let raw = u32::from_str_radix(chunk, 16).ok()?;
        bytes[index * 4..index * 4 + 4].copy_from_slice(&raw.to_le_bytes());
    }
    Some(IpAddr::V6(Ipv6Addr::from(bytes)).to_canonical())
}

/// The listening TCP sockets of both address families, with owning PIDs,
/// read through the IP Helper API.
#[cfg(windows)]
mod windows_tcp {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_SUPPORTED};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    use super::ListenerRow;

    pub fn listeners() -> anyhow::Result<Vec<ListenerRow>> {
        let mut rows = Vec::new();
        let v4 = table(AF_INET)?
            .ok_or_else(|| anyhow::anyhow!("the IPv4 listener table is unavailable"))?;
        // SAFETY: `table` returned a buffer GetExtendedTcpTable filled for
        // AF_INET, whose layout is MIB_TCPTABLE_OWNER_PID.
        let offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        for row in unsafe { entries::<MIB_TCPROW_OWNER_PID>(&v4, offset)? } {
            rows.push(ListenerRow {
                local: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
                port: port(row.dwLocalPort),
                pid: Some(row.dwOwningPid),
            });
        }
        // A machine with the IPv6 stack removed has no IPv6 listeners.
        if let Some(v6) = table(AF_INET6)? {
            // SAFETY: as above, for AF_INET6 and MIB_TCP6TABLE_OWNER_PID.
            let offset = std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table);
            for row in unsafe { entries::<MIB_TCP6ROW_OWNER_PID>(&v6, offset)? } {
                rows.push(ListenerRow {
                    local: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)).to_canonical(),
                    port: port(row.dwLocalPort),
                    pid: Some(row.dwOwningPid),
                });
            }
        }
        Ok(rows)
    }

    /// The port is the low 16 bits of the DWORD, stored in network order.
    fn port(raw: u32) -> u16 {
        let bytes = raw.to_ne_bytes();
        u16::from_be_bytes([bytes[0], bytes[1]])
    }

    /// The raw listener table for one address family, or `None` if the
    /// family is not supported on this machine.
    fn table(family: u16) -> anyhow::Result<Option<Vec<u64>>> {
        let mut bytes = 0u32;
        // The table can grow between the sizing call and the read.
        for _ in 0..4 {
            // u64 storage satisfies the table's alignment.
            let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
            let mut size = u32::try_from(std::mem::size_of_val(buffer.as_slice()))?;
            // SAFETY: `buffer` is writable for `size` bytes.
            let status = unsafe {
                GetExtendedTcpTable(
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    u32::from(family),
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            match status {
                0 => return Ok(Some(buffer)),
                ERROR_INSUFFICIENT_BUFFER => bytes = size,
                ERROR_NOT_SUPPORTED if family == AF_INET6 => return Ok(None),
                error => anyhow::bail!(
                    "GetExtendedTcpTable (address family {family}) failed: {}",
                    std::io::Error::from_raw_os_error(error as i32)
                ),
            }
        }
        anyhow::bail!("GetExtendedTcpTable (address family {family}): table kept growing")
    }

    /// The rows of a table whose header is `dwNumEntries` followed, at
    /// `offset`, by an array of `Row`, bounds-checked against the buffer.
    ///
    /// # Safety
    /// `buffer` must hold a table written by GetExtendedTcpTable whose rows
    /// are `Row`.
    unsafe fn entries<Row: Copy>(buffer: &[u64], offset: usize) -> anyhow::Result<Vec<Row>> {
        let bytes = std::mem::size_of_val(buffer);
        let count = *(buffer.as_ptr() as *const u32) as usize;
        anyhow::ensure!(
            count
                .checked_mul(std::mem::size_of::<Row>())
                .and_then(|rows| rows.checked_add(offset))
                .is_some_and(|needed| needed <= bytes),
            "listener table claims {count} rows beyond its {bytes}-byte buffer"
        );
        let first = (buffer.as_ptr() as *const u8).add(offset) as *const Row;
        Ok(std::slice::from_raw_parts(first, count).to_vec())
    }
}

/// Ports named by a list of upstream target URLs, defaulting by scheme. This
/// is a configuration fingerprint only; which processes serve the targets is
/// decided by [`pids_serving`].
pub fn ports_of_targets(targets: &[String]) -> Vec<u16> {
    let mut ports: Vec<u16> = targets
        .iter()
        .filter_map(|t| {
            let u = reqwest::Url::parse(t).ok()?;
            u.port_or_known_default()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(local: &str, port: u16, pid: u32) -> ListenerRow {
        ListenerRow {
            local: local.parse().unwrap(),
            port,
            pid: Some(pid),
        }
    }

    /// A row as Linux reports it, without an owner.
    fn anonymous(local: &str, port: u16) -> ListenerRow {
        ListenerRow {
            pid: None,
            ..row(local, port, 0)
        }
    }

    /// This machine's only non-loopback interface address in these tests.
    const INTERFACE: &str = "192.0.2.10";

    fn local(target: &str) -> Option<LocalTarget> {
        LocalTarget::parse(target, || HashSet::from([INTERFACE.parse().unwrap()]))
    }

    fn exposure(target: &str, rows: &[ListenerRow]) -> ListenerState {
        local(target).expect("a local target").exposure(rows)
    }

    /// The listener table of a machine with a network-wide llama.cpp on
    /// 8000, an Ollama on IPv4 loopback plus the IPv6 wildcard on 11434, and
    /// an unrelated service.
    fn table() -> Vec<ListenerRow> {
        vec![
            row("0.0.0.0", 8000, 7020),
            row("127.0.0.1", 11434, 8124),
            row("0.0.0.0", 11435, 58448),
            row("::", 11434, 8124),
            row("0.0.0.0", 445, 4),
        ]
    }

    #[test]
    fn only_targets_on_this_machine_are_local() {
        let never = || -> HashSet<IpAddr> { panic!("interfaces are not needed here") };
        for target in [
            "http://127.0.0.1:11434",
            "http://127.1.2.3:11434",
            "http://localhost:11434",
            "http://LOCALHOST.:11434",
            "http://[::1]:11434",
            "http://[::ffff:127.0.0.1]:11434",
            "http://0.0.0.0:11434",
        ] {
            assert!(LocalTarget::parse(target, never).is_some(), "{target}");
        }
        assert_eq!(
            local("http://[::ffff:127.0.0.1]:8000").unwrap(),
            LocalTarget {
                addresses: vec!["127.0.0.1".parse().unwrap()],
                port: 8000
            }
        );
        assert_eq!(local("http://localhost").unwrap().port, 80);
        assert_eq!(
            local(&format!("http://{INTERFACE}:8000"))
                .unwrap()
                .addresses,
            [INTERFACE.parse::<IpAddr>().unwrap()]
        );
        for target in [
            // Another host, including a documentation address.
            "http://192.0.2.1:11434",
            "http://[2001:db8::1]:11434",
            // Container service names and any other host name, even one
            // that might resolve to this machine.
            "http://ollama:11434",
            "http://host.docker.internal:11434",
            "http://gpu-box.lan:8000",
            "http://localhost.example:8000",
            "not a url",
            "unix:/run/ollama.sock",
        ] {
            assert_eq!(local(target), None, "{target}");
        }
    }

    #[test]
    fn exposure_is_that_of_the_listener_serving_the_target() {
        let rows = table();
        assert_eq!(
            exposure("http://127.0.0.1:8000", &rows),
            ListenerState::ListensOnNetwork
        );
        // The IPv4 loopback listener shares its process with an IPv6
        // wildcard, so network exposure wins, in either order.
        assert_eq!(
            exposure("http://127.0.0.1:11434", &rows),
            ListenerState::ListensOnNetwork
        );
        let mut reversed = rows.clone();
        reversed.reverse();
        assert_eq!(
            exposure("http://localhost:11434", &reversed),
            ListenerState::ListensOnNetwork
        );
        assert_eq!(
            exposure("http://127.0.0.1:9999", &rows),
            ListenerState::Unknown
        );
        let only_loopback = [row("127.0.0.1", 11434, 42), row("::1", 11434, 42)];
        assert_eq!(
            exposure("http://localhost:11434", &only_loopback),
            ListenerState::LoopbackOnly
        );
        assert_eq!(
            exposure(
                &format!("http://{INTERFACE}:8000"),
                &[row(INTERFACE, 8000, 1)]
            ),
            ListenerState::ListensOnNetwork
        );
    }

    #[test]
    fn a_listener_that_cannot_accept_the_target_does_not_serve_it() {
        // Bound to an interface address only: a loopback target reaches
        // nothing, so there is no serving listener to describe.
        assert_eq!(
            exposure("http://127.0.0.1:8000", &[row(INTERFACE, 8000, 1)]),
            ListenerState::Unknown
        );
        // An IPv4 wildcard does not accept IPv6 connections...
        assert_eq!(
            exposure("http://[::1]:8000", &[row("0.0.0.0", 8000, 1)]),
            ListenerState::Unknown
        );
        // ...but an IPv6 wildcard may be dual-stack, so it counts for IPv4.
        assert_eq!(
            exposure("http://127.0.0.1:8000", &[row("::", 8000, 1)]),
            ListenerState::ListensOnNetwork
        );
    }

    #[test]
    fn another_process_on_the_same_port_counts_only_without_owners() {
        let owned = [row("127.0.0.1", 7000, 1), row(INTERFACE, 7000, 2)];
        assert_eq!(
            exposure("http://127.0.0.1:7000", &owned),
            ListenerState::LoopbackOnly
        );
        // Without owners the other socket may be the same service, so the
        // reading errs towards network exposure.
        let anonymous_rows = [anonymous("127.0.0.1", 7000), anonymous(INTERFACE, 7000)];
        assert_eq!(
            exposure("http://127.0.0.1:7000", &anonymous_rows),
            ListenerState::ListensOnNetwork
        );
    }

    #[test]
    fn lender_processes_are_those_serving_local_targets() {
        let vllm = local("http://127.0.0.1:8000").unwrap();
        let ollama = local("http://localhost:11434").unwrap();
        assert_eq!(serving_pids(&vllm, &table()), HashSet::from([7020]));
        assert_eq!(serving_pids(&ollama, &table()), HashSet::from([8124]));
        assert!(serving_pids(&vllm, &[]).is_empty());
        // Another process bound only to an interface address on the same
        // port does not serve a loopback target.
        let unrelated = [row(INTERFACE, 8000, 99)];
        assert!(serving_pids(&vllm, &unrelated).is_empty());
    }

    #[test]
    fn target_urls_reduce_to_ports() {
        let targets = vec![
            "http://127.0.0.1:11434".to_string(),
            "http://127.0.0.1:8000".to_string(),
            "http://127.0.0.1:11434/".to_string(), // duplicate
            "http://localhost".to_string(),        // default port for the scheme
            "not a url".to_string(),
        ];
        assert_eq!(ports_of_targets(&targets), vec![80, 8000, 11434]);
    }

    #[test]
    fn remote_targets_are_not_observable_without_a_table_read() {
        // Every target is remote, so no listener table is read: this
        // succeeds even where reading one is unsupported.
        assert_eq!(
            target_listener_states(&[
                "http://192.0.2.1:11434".to_string(),
                "http://ollama:11434".to_string(),
            ]),
            [
                Ok(ListenerState::NotObservable),
                Ok(ListenerState::NotObservable)
            ]
        );
    }

    /// The reported defect: a remote target borrowed the status of an
    /// unrelated local listener on the same port.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_remote_target_never_inherits_a_local_listener_on_its_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let states = target_listener_states(&[
            format!("http://192.0.2.1:{port}"),
            format!("http://ollama:{port}"),
            format!("http://127.0.0.1:{port}"),
        ]);
        assert_eq!(
            states,
            [
                Ok(ListenerState::NotObservable),
                Ok(ListenerState::NotObservable),
                Ok(ListenerState::LoopbackOnly)
            ]
        );
    }

    /// The live table must include a socket this test opens, on both
    /// families, with this process as its owner: the numbers-only read is
    /// the whole point on a non-English Windows.
    #[cfg(windows)]
    #[test]
    fn live_listener_table_sees_our_own_socket() {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        let rows = windows_tcp::listeners().unwrap();
        assert!(
            rows.contains(&row("127.0.0.1", port, std::process::id())),
            "{port} missing from {rows:?}"
        );
        let target = format!("http://127.0.0.1:{port}");
        assert_eq!(
            target_listener_states(std::slice::from_ref(&target)),
            [Ok(ListenerState::LoopbackOnly)]
        );
        assert_eq!(
            pids_serving(&[target, format!("http://192.0.2.1:{port}")]).unwrap(),
            [Some(HashSet::from([std::process::id()])), None]
        );
        if let Ok(v6) = std::net::TcpListener::bind("[::]:0") {
            let port = v6.local_addr().unwrap().port();
            let rows = windows_tcp::listeners().unwrap();
            assert!(
                rows.contains(&row("::", port, std::process::id())),
                "{rows:?}"
            );
            assert_eq!(
                target_listener_states(&[format!("http://[::1]:{port}")]),
                [Ok(ListenerState::ListensOnNetwork)]
            );
        }
    }

    #[test]
    fn linux_proc_listener_rows_handle_ipv4_and_ipv6() {
        let tcp = "  sl  local_address rem_address   st\n   0: 0100007F:2CAA 00000000:0000 0A\n   1: 00000000:1F40 00000000:0000 0A\n   2: 0100007F:1F41 0100007F:D431 01\n";
        let tcp6 = "  sl  local_address rem_address   st\n   0: 00000000000000000000000001000000:2CAA 00000000000000000000000000000000:0000 0A\n   1: 0000000000000000FFFF00000100007F:1F42 00000000000000000000000000000000:0000 0A\n";
        let mut rows = proc_net_rows(tcp, false);
        rows.extend(proc_net_rows(tcp6, true));
        assert_eq!(
            rows,
            [
                anonymous("127.0.0.1", 11434),
                anonymous("0.0.0.0", 8000),
                anonymous("::1", 11434),
                // An IPv4-mapped IPv6 listener is an IPv4 one.
                anonymous("127.0.0.1", 8002),
            ]
        );
        assert_eq!(
            exposure("http://127.0.0.1:8000", &rows),
            ListenerState::ListensOnNetwork
        );
        assert_eq!(
            exposure("http://localhost:11434", &rows),
            ListenerState::LoopbackOnly
        );
        assert_eq!(
            exposure("http://127.0.0.1:9999", &rows),
            ListenerState::Unknown
        );
    }

    #[test]
    fn lender_set_includes_listener_children_and_grandchildren() {
        let table = ProcessTable::from_entries(&[
            (10, 100, None, "ollama"),
            (11, 101, Some(10), "launcher"),
            (12, 102, Some(11), "llama-server"),
            (20, 200, None, "game"),
        ]);
        let lenders = lender_processes(&HashSet::from([10]), &table);

        assert_eq!(
            lenders,
            HashSet::from([
                ProcessIdentity {
                    pid: 10,
                    started_at_s: 100
                },
                ProcessIdentity {
                    pid: 11,
                    started_at_s: 101
                },
                ProcessIdentity {
                    pid: 12,
                    started_at_s: 102
                },
            ])
        );
        assert!(!lenders.contains(&ProcessIdentity {
            pid: 20,
            started_at_s: 200
        }));
    }

    #[test]
    fn a_listener_missing_from_the_process_snapshot_is_not_invented() {
        let table = ProcessTable::from_entries(&[(20, 200, None, "game")]);
        assert!(lender_processes(&HashSet::from([10]), &table).is_empty());
    }

    #[test]
    fn process_snapshot_contains_the_current_process_identity() {
        let table = ProcessTable::capture();
        let pid = std::process::id();
        let identity = table
            .identity(pid)
            .expect("the test process must be present in its own snapshot");
        assert_eq!(identity.pid, pid);
        assert!(identity.started_at_s > 0);
        assert!(table.name(pid).is_some_and(|name| !name.is_empty()));
    }
}
