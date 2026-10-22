//! Stable GPU adapter identity.
//!
//! Vendor probes identify a GPU by PCI address. Windows GPU performance
//! counters identify it by LUID. The D3DKMT adapter inventory is the exact
//! bridge between those namespaces; no load or memory measurement participates
//! in identity.

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A canonical PCI domain:bus:device.function address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PciAddress {
    domain: u16,
    bus: u8,
    device: u8,
    function: u8,
}

impl PciAddress {
    pub const fn new(domain: u16, bus: u8, device: u8, function: u8) -> Self {
        Self {
            domain,
            bus,
            device,
            function,
        }
    }
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.device, self.function
        )
    }
}

impl FromStr for PciAddress {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let (domain, rest) = match raw.split_once(':') {
            Some((first, rest)) if rest.matches(':').count() == 1 => (first, rest),
            _ if raw.matches(':').count() == 1 => ("0", raw),
            _ => bail!("expected PCI address domain:bus:device.function"),
        };
        let (bus, slot) = rest
            .split_once(':')
            .context("expected PCI address domain:bus:device.function")?;
        let (device, function) = slot
            .split_once('.')
            .context("expected PCI address domain:bus:device.function")?;

        let domain = u16::from_str_radix(domain, 16).context("invalid PCI domain")?;
        let bus = u8::from_str_radix(bus, 16).context("invalid PCI bus")?;
        let device = u8::from_str_radix(device, 16).context("invalid PCI device")?;
        let function = u8::from_str_radix(function, 16).context("invalid PCI function")?;
        if device > 0x1f {
            bail!("PCI device must be between 00 and 1f");
        }
        if function > 7 {
            bail!("PCI function must be between 0 and 7");
        }
        Ok(Self::new(domain, bus, device, function))
    }
}

impl Serialize for PciAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PciAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// One exact Windows counter-identity to PCI-identity association.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsAdapter {
    pub luid: String,
    pub pci: PciAddress,
}

/// What `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERADDRESS)` returned for one
/// enumerated adapter, before any interpretation. Kept free of Windows types
/// so the classification below is testable on every platform.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct AdapterAddressRow {
    pub luid: String,
    /// `Err` carries the query's failing NTSTATUS.
    pub address: std::result::Result<RawAdapterAddress, i32>,
}

/// `D3DKMT_ADAPTERADDRESS` as reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RawAdapterAddress {
    pub bus: u32,
    pub device: u32,
    pub function: u32,
}

/// How one reported adapter address is read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum AddressClass {
    /// A real PCI location.
    Pci(PciAddress),
    /// Not a PCI device at all: the Microsoft Basic Render Driver, an
    /// indirect or virtual display adapter. WDDM reports these with the
    /// all-ones bus number, the same sentinel a PCI read of an absent device
    /// returns, and they can never be the NVIDIA card a probe names.
    NotPci,
    /// A value no PCI device can have. This is a genuine error.
    Invalid(String),
}

/// Bus number WDDM reports for an adapter that is not on PCI.
#[cfg_attr(not(windows), allow(dead_code))]
const NOT_PCI_BUS: u32 = u32::MAX;

/// Classify one reported adapter address.
///
/// Only the all-ones bus marks a non-PCI adapter. The device and function
/// numbers are not consulted for it, because they carry no PCI meaning once
/// the bus does not; any other out-of-range field stays an error.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn classify_address(address: RawAdapterAddress) -> AddressClass {
    if address.bus == NOT_PCI_BUS {
        return AddressClass::NotPci;
    }
    let Ok(bus) = u8::try_from(address.bus) else {
        return AddressClass::Invalid(format!("adapter reported invalid PCI bus {}", address.bus));
    };
    let Ok(device) = u8::try_from(address.device) else {
        return AddressClass::Invalid(format!(
            "adapter reported invalid PCI device {}",
            address.device
        ));
    };
    let Ok(function) = u8::try_from(address.function) else {
        return AddressClass::Invalid(format!(
            "adapter reported invalid PCI function {}",
            address.function
        ));
    };
    if device > 0x1f || function > 7 {
        return AddressClass::Invalid(format!(
            "adapter reported invalid PCI address {bus:02x}:{device:02x}.{function:x}"
        ));
    }
    AddressClass::Pci(PciAddress::new(0, bus, device, function))
}

/// The WDDM adapter inventory, with what could not be mapped kept apart
/// from what was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterInventory {
    pub adapters: Vec<WindowsAdapter>,
    /// Adapters that are not PCI devices, by LUID. Expected on every
    /// Windows machine (at least the Basic Render Driver); not an error.
    pub not_pci: Vec<String>,
    /// Adapters whose address could not be read or made no sense, sorted,
    /// so a caller can tell whether the set changed since the last read.
    pub errors: Vec<String>,
}

/// Interpret the queried rows. `Err` when no adapter could be mapped and at
/// least one failed, because the inventory then says nothing usable.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn inventory_from_rows(rows: Vec<AdapterAddressRow>) -> Result<AdapterInventory> {
    let mut inventory = AdapterInventory::default();
    for row in rows {
        match row.address {
            Err(status) => inventory
                .errors
                .push(format!("LUID {} (NTSTATUS {status:#x})", row.luid)),
            Ok(address) => match classify_address(address) {
                AddressClass::Pci(pci) => inventory.adapters.push(WindowsAdapter {
                    luid: row.luid,
                    pci,
                }),
                AddressClass::NotPci => inventory.not_pci.push(row.luid),
                AddressClass::Invalid(error) => {
                    inventory.errors.push(format!("LUID {}: {error}", row.luid))
                }
            },
        }
    }
    inventory.errors.sort();
    if inventory.adapters.is_empty() && !inventory.errors.is_empty() {
        bail!(
            "no adapter address could be queried: {}",
            inventory.errors.join("; ")
        );
    }
    Ok(inventory)
}

/// Enumerate WDDM adapters and query each physical PCI address.
///
/// This does not log: the caller polls it, and decides when a changed set of
/// failures is worth reporting.
#[cfg(windows)]
pub fn windows_adapters() -> Result<AdapterInventory> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::ptr;

    use windows_sys::Wdk::Graphics::Direct3D::{
        D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, D3DKMT_ADAPTERADDRESS,
        D3DKMT_ADAPTERINFO, D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_QUERYADAPTERINFO,
        KMTQAITYPE_ADAPTERADDRESS,
    };

    let mut enumeration = D3DKMT_ENUMADAPTERS2 {
        NumAdapters: 0,
        pAdapters: ptr::null_mut(),
    };
    // SAFETY: the structure is initialized as required for the documented
    // sizing call; the OS writes only NumAdapters while pAdapters is null.
    let status = unsafe { D3DKMTEnumAdapters2(&mut enumeration) };
    if status < 0 {
        bail!("D3DKMTEnumAdapters2 sizing failed with NTSTATUS {status:#x}");
    }
    if enumeration.NumAdapters > 256 {
        bail!(
            "D3DKMTEnumAdapters2 reported an implausible adapter count ({})",
            enumeration.NumAdapters
        );
    }
    if enumeration.NumAdapters == 0 {
        return Ok(AdapterInventory::default());
    }

    // D3DKMT_ADAPTERINFO contains only integer handles/values and a LUID, for
    // which an all-zero bit pattern is valid as an output buffer.
    let mut adapters =
        vec![unsafe { zeroed::<D3DKMT_ADAPTERINFO>() }; enumeration.NumAdapters as usize];
    enumeration.pAdapters = adapters.as_mut_ptr();
    // SAFETY: pAdapters points at NumAdapters writable, correctly aligned
    // D3DKMT_ADAPTERINFO entries for the duration of the call.
    let status = unsafe { D3DKMTEnumAdapters2(&mut enumeration) };
    if status < 0 {
        bail!("D3DKMTEnumAdapters2 failed with NTSTATUS {status:#x}");
    }
    if enumeration.NumAdapters as usize > adapters.len() {
        bail!(
            "D3DKMTEnumAdapters2 returned {} adapters into a {}-entry buffer",
            enumeration.NumAdapters,
            adapters.len()
        );
    }
    adapters.truncate(enumeration.NumAdapters as usize);

    let mut rows = Vec::with_capacity(adapters.len());
    for adapter in adapters {
        // D3DKMT_ADAPTERADDRESS is a plain integer output structure.
        let mut address = unsafe { zeroed::<D3DKMT_ADAPTERADDRESS>() };
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: adapter.hAdapter,
            Type: KMTQAITYPE_ADAPTERADDRESS,
            pPrivateDriverData: (&mut address as *mut D3DKMT_ADAPTERADDRESS).cast::<c_void>(),
            PrivateDriverDataSize: size_of::<D3DKMT_ADAPTERADDRESS>() as u32,
        };
        // SAFETY: query names a live handle returned by enumeration and points
        // to a correctly sized writable D3DKMT_ADAPTERADDRESS.
        let query_status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };

        let close = D3DKMT_CLOSEADAPTER {
            hAdapter: adapter.hAdapter,
        };
        // SAFETY: this closes exactly the live handle returned by enumeration;
        // it is not used again after this call.
        let close_status = unsafe { D3DKMTCloseAdapter(&close) };
        if close_status < 0 {
            tracing::warn!(
                handle = adapter.hAdapter,
                status = format_args!("{close_status:#x}"),
                "could not close enumerated GPU adapter handle"
            );
        }

        rows.push(AdapterAddressRow {
            luid: format_luid(adapter.AdapterLuid.HighPart, adapter.AdapterLuid.LowPart),
            address: if query_status < 0 {
                Err(query_status)
            } else {
                Ok(RawAdapterAddress {
                    bus: address.BusNumber,
                    device: address.DeviceNumber,
                    function: address.FunctionNumber,
                })
            },
        });
    }
    inventory_from_rows(rows)
}

#[cfg(not(windows))]
pub fn windows_adapters() -> Result<AdapterInventory> {
    bail!("Windows GPU adapter identity is unavailable on this platform")
}

#[cfg(windows)]
fn format_luid(high: i32, low: u32) -> String {
    format!("0x{:08x}_0x{low:08x}", high as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_addresses_normalize_vendor_and_short_forms() {
        let long: PciAddress = "00000000:0E:00.0".parse().unwrap();
        let short: PciAddress = "0e:00.0".parse().unwrap();
        assert_eq!(long, short);
        assert_eq!(long.to_string(), "0000:0e:00.0");
    }

    #[test]
    fn pci_addresses_reject_out_of_range_slots() {
        assert!("0000:0e:20.0".parse::<PciAddress>().is_err());
        assert!("0000:0e:00.8".parse::<PciAddress>().is_err());
    }

    fn row(luid: &str, bus: u32, device: u32, function: u32) -> AdapterAddressRow {
        AdapterAddressRow {
            luid: luid.into(),
            address: Ok(RawAdapterAddress {
                bus,
                device,
                function,
            }),
        }
    }

    #[test]
    fn live_65_a_non_pci_adapter_beside_a_real_one_is_not_an_error() {
        // The field report: an NVIDIA card plus an adapter reporting bus
        // 4294967295 (Basic Render Driver or an indirect display).
        let inventory = inventory_from_rows(vec![
            row("0x00000000_0x00013fa9", 0x0e, 0, 0),
            row("0x00000000_0x0000d1b2", u32::MAX, u32::MAX, u32::MAX),
        ])
        .unwrap();
        assert_eq!(
            inventory.adapters,
            vec![WindowsAdapter {
                luid: "0x00000000_0x00013fa9".into(),
                pci: "0000:0e:00.0".parse().unwrap(),
            }]
        );
        assert_eq!(inventory.not_pci, vec!["0x00000000_0x0000d1b2".to_string()]);
        assert!(inventory.errors.is_empty(), "{:?}", inventory.errors);
    }

    #[test]
    fn the_all_ones_bus_alone_marks_a_non_pci_adapter() {
        for (device, function) in [(u32::MAX, u32::MAX), (0, 0), (u32::MAX, 0)] {
            assert_eq!(
                classify_address(RawAdapterAddress {
                    bus: u32::MAX,
                    device,
                    function,
                }),
                AddressClass::NotPci
            );
        }
    }

    #[test]
    fn genuine_out_of_range_addresses_stay_errors() {
        for (bus, device, function, message) in [
            (256, 0, 0, "invalid PCI bus 256"),
            (u32::MAX - 1, 0, 0, "invalid PCI bus 4294967294"),
            (1, 256, 0, "invalid PCI device 256"),
            (1, u32::MAX, 0, "invalid PCI device 4294967295"),
            (1, 0, 256, "invalid PCI function 256"),
            (1, 0x20, 0, "invalid PCI address 01:20.0"),
            (1, 0, 8, "invalid PCI address 01:00.8"),
        ] {
            match classify_address(RawAdapterAddress {
                bus,
                device,
                function,
            }) {
                AddressClass::Invalid(error) => assert!(error.contains(message), "{error}"),
                other => panic!("{bus}:{device}.{function} classified as {other:?}"),
            }
        }
        assert_eq!(
            classify_address(RawAdapterAddress {
                bus: 0xff,
                device: 0x1f,
                function: 7,
            }),
            AddressClass::Pci(PciAddress::new(0, 0xff, 0x1f, 7))
        );
    }

    #[test]
    fn errors_are_kept_sorted_beside_mapped_adapters() {
        let inventory = inventory_from_rows(vec![
            row("0x0_0x3", 300, 0, 0),
            AdapterAddressRow {
                luid: "0x0_0x2".into(),
                address: Err(-1073741823),
            },
            row("0x0_0x1", 1, 0, 0),
        ])
        .unwrap();
        assert_eq!(inventory.adapters.len(), 1);
        assert_eq!(
            inventory.errors,
            vec![
                "LUID 0x0_0x2 (NTSTATUS 0xc0000001)".to_string(),
                "LUID 0x0_0x3: adapter reported invalid PCI bus 300".to_string(),
            ]
        );
    }

    #[test]
    fn only_non_pci_adapters_is_an_empty_inventory_not_a_failure() {
        let inventory =
            inventory_from_rows(vec![row("0x0_0x1", u32::MAX, u32::MAX, u32::MAX)]).unwrap();
        assert!(inventory.adapters.is_empty());
        assert!(inventory.errors.is_empty());
        assert_eq!(inventory.not_pci.len(), 1);
    }

    #[test]
    fn nothing_mapped_and_something_failed_is_a_failure() {
        let error = inventory_from_rows(vec![
            row("0x0_0x1", u32::MAX, u32::MAX, u32::MAX),
            row("0x0_0x2", 999, 0, 0),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("invalid PCI bus 999"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn luids_match_the_performance_counter_namespace() {
        assert_eq!(format_luid(0, 0x13fa9), "0x00000000_0x00013fa9");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a real Windows session with a WDDM adapter"]
    fn live_windows_inventory_has_exact_addresses() {
        let inventory = windows_adapters().expect("D3DKMT adapter discovery should succeed");
        assert!(inventory.errors.is_empty(), "{:?}", inventory.errors);
        let adapters = inventory.adapters;
        assert!(
            !adapters.is_empty(),
            "at least one WDDM adapter should exist"
        );
        assert!(adapters
            .iter()
            .all(|adapter| adapter.luid.starts_with("0x")));
    }
}
