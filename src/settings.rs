//! Whitelisted settings shared by the daemon and owner console.
//! Credential contents and installation paths never cross this boundary.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    pub revision: String,
    pub owner: OwnerSettings,
    pub resources: Vec<ResourceSetting>,
    pub upstreams: Vec<UpstreamSetting>,
    pub poll_interval_ms: u64,
    pub server_listen: String,
    pub server_limits: ServerLimitsSettings,
    pub application: SettingsApplication,
    pub capabilities: SettingsCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub revision: String,
    pub owner: OwnerSettings,
    pub resources: Vec<ResourceSetting>,
    pub upstreams: Vec<UpstreamSetting>,
    pub poll_interval_ms: u64,
    pub server_listen: String,
    pub server_limits: ServerLimitsSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsApplication {
    pub capacity_revision: Option<String>,
    pub restart_required: bool,
}

/// Read-only configuration status. It does not attest credential resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsCapabilities {
    pub auth_tokens_configured: u64,
    pub upstream_credentials_configured: u64,
    pub trust_loopback: bool,
    pub allow_unauthenticated_remote: bool,
    pub local_admin_channel: String,
    pub local_admin_group_configured: bool,
    pub custom_admission_storage: bool,
    pub custom_pressure_cache: bool,
    pub custom_admin_socket: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSettings {
    pub unlocked_session_is_presence: bool,
    pub idle_lends_after_s: u64,
    pub media_is_presence: bool,
    pub release_after_s: u64,
    pub processes: Vec<String>,
    pub media_ignore: Vec<String>,
    pub sign_in_screen_is_presence: bool,
    pub presence: String,
    pub media_poll_ms: u64,
    pub gpu_pressure: GpuPressureSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuPressureSettings {
    pub enabled: bool,
    pub mode: String,
    pub threshold_pct: f64,
    pub sustain_s: u64,
    pub ignore: Vec<String>,
    pub cap_pct: f64,
    pub margin_pct: f64,
    pub floor_pct: f64,
    pub learn_quantile: f64,
    pub learn_half_life_h: f64,
    pub raise_pct_per_h: f64,
    pub warmup_samples: u64,
    pub poll_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSetting {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    pub reserve: Option<String>,
    pub selector: Option<String>,
    pub adapter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSetting {
    pub id: String,
    pub kind: String,
    pub yield_on_veto: bool,
    pub on_owner_return: String,
    pub drain_timeout_s: u64,
    pub owner_scope: String,
    pub management: String,
    pub completion_timeout_s: u64,
    pub forward_authorization: bool,
    pub listen: String,
    pub target: String,
    pub resource: String,
    pub also_requires: Vec<String>,
    pub default_min_free_mib: String,
    pub models: Vec<ModelSetting>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSetting {
    pub name: String,
    pub mode: String,
    pub min_free_mib: String,
    pub resident_min_free_mib: String,
    pub resources: Vec<ModelResourceSetting>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResourceSetting {
    pub id: String,
    pub min_free_mib: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerLimitsSettings {
    pub header_read_timeout_s: u64,
    pub body_read_timeout_s: u64,
    pub body_idle_timeout_s: u64,
    pub write_stall_timeout_s: u64,
    pub max_connections: u64,
    pub max_request_body_mib: u64,
    pub max_buffered_body_mib: Option<u64>,
    pub tunnel_idle_timeout_s: u64,
}

impl From<SettingsSnapshot> for SettingsUpdate {
    fn from(snapshot: SettingsSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            owner: snapshot.owner,
            resources: snapshot.resources,
            upstreams: snapshot.upstreams,
            poll_interval_ms: snapshot.poll_interval_ms,
            server_listen: snapshot.server_listen,
            server_limits: snapshot.server_limits,
        }
    }
}
