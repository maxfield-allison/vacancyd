// Generated from src/settings.rs by generate-settings.mjs.
export type SettingsSnapshot = {
  revision: string;
  owner: OwnerSettings;
  resources: (ResourceSetting)[];
  upstreams: (UpstreamSetting)[];
  poll_interval_ms: number;
  server_listen: string;
  server_limits: ServerLimitsSettings;
  application: SettingsApplication;
  capabilities: SettingsCapabilities;
};

export type SettingsUpdate = {
  revision: string;
  owner: OwnerSettings;
  resources: (ResourceSetting)[];
  upstreams: (UpstreamSetting)[];
  poll_interval_ms: number;
  server_listen: string;
  server_limits: ServerLimitsSettings;
};

export type SettingsApplication = {
  capacity_revision: string | null;
  restart_required: boolean;
};

export type SettingsCapabilities = {
  auth_tokens_configured: number;
  upstream_credentials_configured: number;
  trust_loopback: boolean;
  allow_unauthenticated_remote: boolean;
  local_admin_channel: string;
  local_admin_group_configured: boolean;
  custom_admission_storage: boolean;
  custom_pressure_cache: boolean;
  custom_admin_socket: boolean;
};

export type OwnerSettings = {
  unlocked_session_is_presence: boolean;
  idle_lends_after_s: number;
  media_is_presence: boolean;
  release_after_s: number;
  processes: (string)[];
  media_ignore: (string)[];
  sign_in_screen_is_presence: boolean;
  presence: string;
  media_poll_ms: number;
  gpu_pressure: GpuPressureSettings;
};

export type GpuPressureSettings = {
  enabled: boolean;
  mode: string;
  threshold_pct: number;
  sustain_s: number;
  ignore: (string)[];
  cap_pct: number;
  margin_pct: number;
  floor_pct: number;
  learn_quantile: number;
  learn_half_life_h: number;
  raise_pct_per_h: number;
  warmup_samples: number;
  poll_ms: number;
};

export type ResourceSetting = {
  id: string;
  kind: string;
  enabled: boolean;
  reserve: string | null;
  selector: string | null;
  adapter: string | null;
};

export type UpstreamSetting = {
  id: string;
  kind: string;
  yield_on_veto: boolean;
  on_owner_return: string;
  drain_timeout_s: number;
  owner_scope: string;
  management: string;
  completion_timeout_s: number;
  forward_authorization: boolean;
  listen: string;
  target: string;
  resource: string;
  also_requires: (string)[];
  default_min_free_mib: string;
  models: (ModelSetting)[];
};

export type ModelSetting = {
  name: string;
  mode: string;
  min_free_mib: string;
  resident_min_free_mib: string;
  resources: (ModelResourceSetting)[];
};

export type ModelResourceSetting = {
  id: string;
  min_free_mib: string;
};

export type ServerLimitsSettings = {
  header_read_timeout_s: number;
  body_read_timeout_s: number;
  body_idle_timeout_s: number;
  write_stall_timeout_s: number;
  max_connections: number;
  max_request_body_mib: number;
  max_buffered_body_mib: number | null;
  tunnel_idle_timeout_s: number;
};

