//! Prometheus metrics. Everything vacancyd knows is exported here so the
//! borrowing side can see the lender's state continuously, not just at
//! request time.

use prometheus::{
    Encoder, GaugeVec, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry, TextEncoder,
};

mod gpu;

pub struct Metrics {
    pub registry: Registry,
    pub gpu: gpu::GpuMetrics,
    pub resource_free_bytes: IntGaugeVec,
    pub resource_total_bytes: IntGaugeVec,
    pub resource_utilization_pct: IntGaugeVec,
    pub resource_enabled: IntGaugeVec,
    /// Exactly one state is 1 for each resource/input pair. Freshness is
    /// computed at scrape time so a stopped collector becomes stale visibly.
    pub resource_readiness: IntGaugeVec,
    /// Age of the last successful input observation, or -1 before the first.
    pub resource_observation_age_seconds: IntGaugeVec,
    /// Percent of each resource used by processes outside the lender set.
    /// Exported with its learned threshold so the decision is explainable.
    pub resource_pressure_pct: GaugeVec,
    /// The engage threshold currently in force per resource and profile.
    pub resource_pressure_threshold_pct: GaugeVec,
    /// The learned quantile per resource and profile; absent until that
    /// profile's histogram holds `warmup_samples` readings, and in fixed
    /// mode.
    pub resource_pressure_baseline_pct: GaugeVec,
    /// The threshold the learned quantile asks for, which the threshold in
    /// force approaches; absent under the same conditions as the baseline.
    pub resource_pressure_target_pct: GaugeVec,
    /// 1 while pressure refuses the resource, on the attendance profile of
    /// the last sample; the other profile has no series.
    pub resource_pressure_engaged: IntGaugeVec,
    pub resource_calibrated: IntGaugeVec,
    pub resource_calibration_samples: IntGaugeVec,
    pub owner_veto_active: IntGauge,
    /// 1 for each upstream the owner veto does not apply to
    /// (`owner_scope = "resource"`), 0 for the rest. Set once at startup.
    pub upstream_owner_veto_exempt: IntGaugeVec,
    pub session_state: IntGaugeVec,
    pub agent_connected: IntGauge,
    pub agent_active_connections: IntGauge,
    /// Incremented by the agent channel, which is Windows-only today.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub agent_reports_total: prometheus::IntCounter,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub agent_rejections_total: IntCounterVec,
    pub owner_idle_seconds: IntGauge,
    pub media_playing: IntGauge,
    pub lending_enabled: IntGauge,
    pub requests_total: IntCounterVec,
    pub auth_total: IntCounterVec,
    /// Current admission ledger entries by upstream and durable phase.
    pub active_work: IntGaugeVec,
    pub yields_total: IntCounterVec,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let gpu = gpu::GpuMetrics::new(&registry);
        let resource_free_bytes = IntGaugeVec::new(
            Opts::new("vacancyd_resource_free_bytes", "Free capacity per resource"),
            &["resource"],
        )
        .unwrap();
        let resource_total_bytes = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_total_bytes",
                "Total capacity per resource",
            ),
            &["resource"],
        )
        .unwrap();
        let resource_utilization_pct = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_utilization_pct",
                "Utilization percent per resource; -1 when unavailable",
            ),
            &["resource"],
        )
        .unwrap();
        let resource_pressure_pct = GaugeVec::new(
            Opts::new(
                "vacancyd_resource_pressure_pct",
                "Utilization percent from processes outside the lender set",
            ),
            &["resource"],
        )
        .unwrap();
        let resource_pressure_threshold_pct = GaugeVec::new(
            Opts::new(
                "vacancyd_resource_pressure_threshold_pct",
                "Pressure engage threshold in force for the attendance profile",
            ),
            &["resource", "profile"],
        )
        .unwrap();
        let resource_pressure_baseline_pct = GaugeVec::new(
            Opts::new(
                "vacancyd_resource_pressure_baseline_pct",
                "Learned pressure quantile for the attendance profile; absent until its histogram is warm",
            ),
            &["resource", "profile"],
        )
        .unwrap();
        let resource_pressure_target_pct = GaugeVec::new(
            Opts::new(
                "vacancyd_resource_pressure_target_pct",
                "Threshold the learned quantile asks for; the threshold in force rises toward it at a bounded rate and falls to it at once",
            ),
            &["resource", "profile"],
        )
        .unwrap();
        let resource_pressure_engaged = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_pressure_engaged",
                "1 while pressure refuses the resource, labelled with the attendance profile of the last sample",
            ),
            &["resource", "profile"],
        )
        .unwrap();
        let resource_enabled = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_enabled",
                "1 when the resource participates in lending",
            ),
            &["resource"],
        )
        .unwrap();
        let resource_readiness = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_readiness",
                "1 for the current readiness state of each resource input",
            ),
            &["resource", "input", "state"],
        )
        .unwrap();
        let resource_observation_age_seconds = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_observation_age_seconds",
                "Age of the last successful resource input observation (-1 before the first)",
            ),
            &["resource", "input"],
        )
        .unwrap();
        let session_state = IntGaugeVec::new(
            Opts::new(
                "vacancyd_session_state",
                "1 for the current console session state (active/locked/disconnected/logged_out/unknown)",
            ),
            &["state"],
        )
        .unwrap();
        let agent_connected = IntGauge::new(
            "vacancyd_agent_connected",
            "1 while a fresh user-session agent report is available",
        )
        .unwrap();
        let agent_reports_total = prometheus::IntCounter::new(
            "vacancyd_agent_reports_total",
            "Reports received from user-session agents",
        )
        .unwrap();
        let agent_active_connections = IntGauge::new(
            "vacancyd_agent_active_connections",
            "Current user-session agent pipe connections",
        )
        .unwrap();
        let agent_rejections_total = IntCounterVec::new(
            Opts::new(
                "vacancyd_agent_rejections_total",
                "Rejected agent connections and reports by fixed reason",
            ),
            &["reason"],
        )
        .unwrap();
        let owner_idle_seconds = IntGauge::new(
            "vacancyd_owner_idle_seconds",
            "Seconds since the owner last touched the machine (-1 when unknown)",
        )
        .unwrap();
        let media_playing = IntGauge::new(
            "vacancyd_media_playing",
            "1 while playback holds the display awake, 0 when checked and quiet, -1 when not checked or unreadable",
        )
        .unwrap();
        let owner_veto_active = IntGauge::new(
            "vacancyd_owner_veto_active",
            "1 while the owner veto is active: host-scoped upstreams refuse, exempt ones (vacancyd_upstream_owner_veto_exempt) may still lend",
        )
        .unwrap();
        let upstream_owner_veto_exempt = IntGaugeVec::new(
            Opts::new(
                "vacancyd_upstream_owner_veto_exempt",
                "1 if the owner veto does not apply to this upstream (owner_scope = resource), 0 if it does",
            ),
            &["upstream"],
        )
        .unwrap();
        let lending_enabled = IntGauge::new(
            "vacancyd_lending_enabled",
            "1 while the global lending switch is on",
        )
        .unwrap();
        let auth_total = IntCounterVec::new(
            Opts::new(
                "vacancyd_auth_total",
                "Authorisation decisions by scope and outcome (allowed/missing/invalid/forbidden)",
            ),
            &["scope", "outcome"],
        )
        .unwrap();
        let requests_total = IntCounterVec::new(
            Opts::new(
                "vacancyd_requests_total",
                "Proxied requests by upstream and outcome",
            ),
            &["upstream", "outcome"],
        )
        .unwrap();
        let active_work = IntGaugeVec::new(
            Opts::new(
                "vacancyd_active_work",
                "Current admitted work by upstream and lifecycle phase",
            ),
            &["upstream", "phase"],
        )
        .unwrap();
        let yields_total = IntCounterVec::new(
            Opts::new(
                "vacancyd_yields_total",
                "Acknowledged model-unload operations; not measured memory recovery",
            ),
            &["upstream"],
        )
        .unwrap();

        let resource_calibrated = IntGaugeVec::new(
            Opts::new("vacancyd_resource_calibrated", "Completed pressure calibration by resource and attendance profile; freshness is reported separately"),
            &["resource", "profile"],
        ).unwrap();
        let resource_calibration_samples = IntGaugeVec::new(
            Opts::new(
                "vacancyd_resource_calibration_samples",
                "Pressure calibration sample progress, capped at the required count",
            ),
            &["resource", "profile"],
        )
        .unwrap();
        for c in [
            registry.register(Box::new(resource_calibrated.clone())),
            registry.register(Box::new(resource_calibration_samples.clone())),
            registry.register(Box::new(resource_free_bytes.clone())),
            registry.register(Box::new(resource_total_bytes.clone())),
            registry.register(Box::new(resource_utilization_pct.clone())),
            registry.register(Box::new(resource_enabled.clone())),
            registry.register(Box::new(resource_readiness.clone())),
            registry.register(Box::new(resource_observation_age_seconds.clone())),
            registry.register(Box::new(resource_pressure_pct.clone())),
            registry.register(Box::new(resource_pressure_threshold_pct.clone())),
            registry.register(Box::new(resource_pressure_baseline_pct.clone())),
            registry.register(Box::new(resource_pressure_target_pct.clone())),
            registry.register(Box::new(resource_pressure_engaged.clone())),
            registry.register(Box::new(session_state.clone())),
            registry.register(Box::new(agent_connected.clone())),
            registry.register(Box::new(agent_active_connections.clone())),
            registry.register(Box::new(agent_reports_total.clone())),
            registry.register(Box::new(agent_rejections_total.clone())),
            registry.register(Box::new(owner_idle_seconds.clone())),
            registry.register(Box::new(media_playing.clone())),
            registry.register(Box::new(owner_veto_active.clone())),
            registry.register(Box::new(upstream_owner_veto_exempt.clone())),
            registry.register(Box::new(lending_enabled.clone())),
            registry.register(Box::new(requests_total.clone())),
            registry.register(Box::new(auth_total.clone())),
            registry.register(Box::new(active_work.clone())),
            registry.register(Box::new(yields_total.clone())),
        ] {
            c.expect("metric registration");
        }

        Self {
            registry,
            gpu,
            resource_free_bytes,
            resource_total_bytes,
            resource_utilization_pct,
            resource_pressure_pct,
            resource_pressure_threshold_pct,
            resource_pressure_baseline_pct,
            resource_pressure_target_pct,
            resource_pressure_engaged,
            resource_calibrated,
            resource_calibration_samples,
            resource_enabled,
            resource_readiness,
            resource_observation_age_seconds,
            session_state,
            agent_connected,
            agent_active_connections,
            agent_reports_total,
            agent_rejections_total,
            owner_idle_seconds,
            media_playing,
            owner_veto_active,
            upstream_owner_veto_exempt,
            lending_enabled,
            requests_total,
            auth_total,
            active_work,
            yields_total,
        }
    }

    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .ok();
        String::from_utf8(buf).unwrap_or_default()
    }
}
