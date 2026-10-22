//! Residency credit: a model the upstream already holds is charged its
//! resident requirement, never less than proof allows.
use super::*;
use crate::admission::{reserve, reserve_for, reserve_with_fresh_observations};
use crate::plugin::adapter::{Completion, RouteFamily, RunnerRequest};
use std::sync::Arc;
use std::time::Instant;

/// `/api/ps` from the live report: gemma3:12b at 8,107,684,003 bytes,
/// fully in VRAM, loaded with a 65536 context.
const GEMMA_BYTES: u64 = 8_107_684_003;

fn state_with(kind: &str, extra: &str) -> Arc<AppState> {
    let config = toml::from_str(&format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        [owner.gpu_pressure]
        enabled = false
        [[resource]]
        id = "gpu0"
        kind = "nvidia-gpu"
        selector = "0"
        [[resource]]
        id = "gpu1"
        kind = "nvidia-gpu"
        selector = "1"
        [[resource]]
        id = "mem"
        kind = "system-memory"
        [[upstream]]
        id = "up"
        listen = "127.0.0.1:0"
        target = "http://127.0.0.1:1"
        kind = "{kind}"
        resource = "gpu0"
        {extra}
        "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    for id in ["gpu1", "mem"] {
        set_free(&state, id, 64 * 1024);
    }
    state
}

fn ollama(models: &str) -> Arc<AppState> {
    state_with("ollama", &format!("[upstream.models]\n{models}"))
}

#[test]
fn voice_engine_switch_cannot_borrow_the_loaded_engines_credit() {
    let state = state_with(
        "chatterbox",
        r#"
        default_min_free_mib = 11500
        [upstream.models]
        qwen = { min_free_mib = 6000, resident_min_free_mib = 1024 }
        breeze = { min_free_mib = 11500, resident_min_free_mib = 1024 }
    "#,
    );
    set_free(&state, "gpu0", 2000);
    observe(&state, vec![loaded("qwen", None, None)]);
    let upstream = &state.config.upstreams[0];
    assert!(admit(&state, upstream, Some("qwen")).is_ok());
    let refusal = admit(&state, upstream, Some("breeze")).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
    assert!(refusal.detail.contains("11500 MiB"), "{}", refusal.detail);
    assert!(admit(&state, upstream, None).is_err());
    assert!(admit(&state, upstream, Some("default")).is_err());
    // A non-exact key must not alias another engine's configured budget.
    assert!(model_requirement(&state, upstream, "qwen-design").is_none());
}

fn set_free(state: &AppState, id: &str, mib: u64) {
    let resource = &state.resources[id];
    let mut observation = resource.capacity.write().unwrap();
    resource.probe_lendable.store(true, Ordering::Relaxed);
    observation.record_success(Capacity {
        free_bytes: mib * MIB,
        total_bytes: 16303 * MIB,
        ..Capacity::default()
    });
}

/// RFC 3339 for `seconds` from now, in UTC.
fn rfc3339_in(seconds: i64) -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + seconds;
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.123456789Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn loaded(name: &str, context: Option<u64>, expires_at: Option<String>) -> vacancyd::status::Model {
    vacancyd::status::Model {
        name: Some(name.into()),
        state: "loaded_idle".into(),
        resident_bytes: Some(GEMMA_BYTES),
        expires_at,
        context_length: context,
        digest: None,
        default_context_length: None,
    }
}

fn observe(state: &AppState, models: Vec<vacancyd::status::Model>) {
    observe_at(state, models, Instant::now());
}

fn observe_at(state: &AppState, models: Vec<vacancyd::status::Model>, at: Instant) {
    state.upstreams["up"]
        .models
        .write()
        .unwrap()
        .record_success_at(
            crate::lifecycle::Models {
                models,
                busy: false,
            },
            at,
        );
}

fn gemma_resident(state: &AppState) {
    observe(
        state,
        vec![loaded("gemma3:12b", Some(65536), Some(rfc3339_in(290)))],
    );
}

fn pinned(model: &str, context: u64) -> Demand<'_> {
    asking(model, RouteFamily::Native, Some(context))
}

fn asking(model: &str, family: RouteFamily, num_ctx: Option<u64>) -> Demand<'_> {
    Demand {
        model: Some(model),
        runner: Some(RunnerRequest { family, num_ctx }),
    }
}

fn try_reserve(state: &Arc<AppState>, demand: Demand<'_>) -> Result<(), Refusal> {
    let upstream = state.config.upstreams[0].clone();
    reserve_for(state, &upstream, demand, Completion::ResponseClose, None).map(|ticket| {
        ticket.response_complete();
    })
}

fn decide(state: &AppState, demand: Demand<'_>) -> Result<Admission, Refusal> {
    let ledger = state.admission.lock().unwrap();
    check(
        state,
        &ledger,
        &Snapshot::read(state),
        &state.config.upstreams[0],
        demand,
    )
}

#[test]
fn live_54_resident_gemma_is_admitted_with_473_mib_free() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);

    let admission = decide(&state, pinned("gemma3:12b", 65536)).unwrap();
    let note = admission.residency.unwrap();
    assert!(
        note.contains("model gemma3:12b is resident with context 65536"),
        "{note}"
    );
    assert!(
        note.contains("charged 0 MiB instead of 8200 MiB on 'gpu0'"),
        "{note}"
    );
    try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap();
}

#[test]
fn a_request_that_may_reload_is_charged_a_cold_load() {
    // The reviewer's reproducer: without proof that the request reuses the
    // runner as loaded, the full requirement still applies.
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);
    let refusal = decide(&state, Demand::model(Some("gemma3:12b"))).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
    assert!(
        refusal
            .detail
            .contains("needs 8200 MiB free on 'gpu0', only 473 MiB free"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal
            .detail
            .contains("a runner option other than num_ctx"),
        "{}",
        refusal.detail
    );
}

/// Admit a request like `demand` with room to load, finish it, then observe
/// the model resident with `context` as the upstream would afterwards.
fn load_and_observe(state: &Arc<AppState>, demand: Demand<'_>, context: u64) {
    set_free(state, "gpu0", 9000);
    let upstream = state.config.upstreams[0].clone();
    reserve_for(state, &upstream, demand, Completion::ResponseClose, None)
        .unwrap()
        .response_complete();
    std::thread::sleep(Duration::from_millis(2));
    observe(
        state,
        vec![loaded("gemma3:12b", Some(context), Some(rfc3339_in(290)))],
    );
    crate::lifecycle::sample_footprints(state, "up");
    set_free(state, "gpu0", 473);
}

fn learned(state: &AppState, runner: RunnerRequest) -> Option<u64> {
    state
        .admission
        .lock()
        .unwrap()
        .learned_context("up", "gemma3:12b", runner)
        .map(|learned| learned.context_length)
}

#[test]
fn an_unset_num_ctx_is_credited_after_a_like_request_loaded_the_resident_context() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    let unset = asking("gemma3:12b", RouteFamily::Native, None);
    load_and_observe(&state, unset, 4096);
    let admission = decide(&state, unset).unwrap();
    assert!(
        admission.residency.as_deref().unwrap().contains(
            "resident with context 4096, which requests with an unset num_ctx were seen to load"
        ),
        "{admission:?}"
    );
    try_reserve(&state, unset).unwrap();
}

#[test]
fn an_unset_num_ctx_with_nothing_learned_is_charged_a_cold_load() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);
    let refusal = decide(&state, asking("gemma3:12b", RouteFamily::Native, None)).unwrap_err();
    assert!(
        refusal.detail.contains("needs 8200 MiB"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal
            .detail
            .contains("no earlier request with an unset num_ctx has shown"),
        "{}",
        refusal.detail
    );
}

#[test]
fn a_learned_context_that_differs_from_the_resident_one_is_charged_a_cold_load() {
    // Requests with an unset num_ctx loaded 8192; the owner has since
    // reloaded the model with 65536, so such a request would reload it.
    let state = ollama(r#""gemma3:12b" = 8200"#);
    let unset = asking("gemma3:12b", RouteFamily::Native, None);
    load_and_observe(&state, unset, 8192);
    gemma_resident(&state);
    let refusal = decide(&state, unset).unwrap_err();
    assert!(
        refusal.detail.contains("needs 8200 MiB"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal.detail.contains(
            "requests with an unset num_ctx loaded context 8192, but the resident context is 65536"
        ),
        "{}",
        refusal.detail
    );
}

#[test]
fn an_explicit_num_ctx_is_credited_for_its_learned_parallel_multiple() {
    // With four parallel slots Ollama reports num_ctx x 4 in /api/ps.
    let state = ollama(r#""gemma3:12b" = 8200"#);
    load_and_observe(&state, pinned("gemma3:12b", 8192), 32768);
    decide(&state, pinned("gemma3:12b", 8192)).unwrap();
    // Another explicit value has no learned multiple and still differs.
    let refusal = decide(&state, pinned("gemma3:12b", 4096)).unwrap_err();
    assert!(
        refusal
            .detail
            .contains("num_ctx 4096 differs from the resident context 32768"),
        "{}",
        refusal.detail
    );
}

#[test]
fn an_openai_route_request_is_credited_after_an_openai_route_load_only() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    let v1 = asking("gemma3:12b", RouteFamily::OpenAi, None);
    let native = asking("gemma3:12b", RouteFamily::Native, None);
    load_and_observe(&state, v1, 4096);
    decide(&state, v1).unwrap();
    // What /v1 taught is not applied to the native API.
    assert!(decide(&state, native).is_err());
    assert_eq!(learned(&state, native.runner.unwrap()), None);
}

#[test]
fn a_new_model_digest_forgets_what_was_learned() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    let unset = asking("gemma3:12b", RouteFamily::Native, None);
    let row = |digest: &str| vacancyd::status::Model {
        digest: Some(digest.into()),
        default_context_length: None,
        ..loaded("gemma3:12b", Some(4096), Some(rfc3339_in(290)))
    };
    set_free(&state, "gpu0", 9000);
    let upstream = state.config.upstreams[0].clone();
    reserve_for(&state, &upstream, unset, Completion::ResponseClose, None)
        .unwrap()
        .response_complete();
    std::thread::sleep(Duration::from_millis(2));
    observe(&state, vec![row("aaaa")]);
    crate::lifecycle::sample_footprints(&state, "up");
    set_free(&state, "gpu0", 473);
    decide(&state, unset).unwrap();
    // A pull replaced the model under the same name.
    observe(&state, vec![row("bbbb")]);
    crate::lifecycle::sample_footprints(&state, "up");
    assert_eq!(learned(&state, unset.runner.unwrap()), None);
    assert!(decide(&state, unset).is_err());
}

#[test]
fn only_an_observation_before_the_next_admission_teaches_a_context() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    let unset = asking("gemma3:12b", RouteFamily::Native, None);
    let upstream = state.config.upstreams[0].clone();
    set_free(&state, "gpu0", 9000);
    reserve_for(&state, &upstream, unset, Completion::ResponseClose, None)
        .unwrap()
        .response_complete();
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 9000);
    // A later request is admitted before any observation; it may reload
    // the model with its own context, so nothing is learned for the first.
    let later = reserve_for(
        &state,
        &upstream,
        pinned("gemma3:12b", 65536),
        Completion::ResponseClose,
        None,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(2));
    gemma_resident(&state);
    crate::lifecycle::sample_footprints(&state, "up");
    assert_eq!(learned(&state, unset.runner.unwrap()), None);
    later.response_complete();

    // A request whose submission was rejected loaded nothing and teaches
    // nothing either.
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 9000);
    gemma_resident(&state);
    reserve_for(&state, &upstream, unset, Completion::ResponseClose, None)
        .unwrap()
        .submission_rejected();
    std::thread::sleep(Duration::from_millis(2));
    gemma_resident(&state);
    crate::lifecycle::sample_footprints(&state, "up");
    assert_eq!(learned(&state, unset.runner.unwrap()), None);
}

#[test]
fn a_different_num_ctx_is_charged_a_cold_load() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);
    let refusal = decide(&state, pinned("gemma3:12b", 8192)).unwrap_err();
    assert!(
        refusal.detail.contains("needs 8200 MiB"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal
            .detail
            .contains("num_ctx 8192 differs from the resident context 65536"),
        "{}",
        refusal.detail
    );

    // A row without a reported context cannot be matched either.
    observe(
        &state,
        vec![loaded("gemma3:12b", None, Some(rfc3339_in(290)))],
    );
    let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert!(
        refusal.detail.contains("context length"),
        "{}",
        refusal.detail
    );
}

#[test]
fn resident_min_free_mib_is_charged_in_place_of_the_load_requirement() {
    let state = ollama(r#""gemma3:12b" = { min_free_mib = 8200, resident_min_free_mib = 500 }"#);
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);
    let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
    assert!(
        refusal.detail.starts_with("model gemma3:12b is resident"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal.detail.contains(
            "still needs 500 MiB free on 'gpu0', only 473 MiB free (a cold load needs 8200 MiB)"
        ),
        "{}",
        refusal.detail
    );
    set_free(&state, "gpu0", 600);
    let admission = decide(&state, pinned("gemma3:12b", 65536)).unwrap();
    assert!(admission
        .residency
        .unwrap()
        .contains("charged 500 MiB instead of 8200 MiB on 'gpu0'"));
}

#[test]
fn the_credit_covers_every_budgeted_dependency_and_never_exceeds_a_cold_load() {
    let state = state_with(
        "ollama",
        r#"also_requires = ["mem"]
        [upstream.models]
        "gemma3:12b" = { gpu0 = 8200, mem = 200, resident_min_free_mib = 300 }"#,
    );
    set_free(&state, "gpu0", 350);
    set_free(&state, "mem", 250);
    gemma_resident(&state);
    let note = decide(&state, pinned("gemma3:12b", 65536))
        .unwrap()
        .residency
        .unwrap();
    // gpu0 is charged 300 instead of 8200; mem stays at its 200 because a
    // credit never charges more than a cold load.
    assert!(
        note.contains("300 MiB instead of 8200 MiB on 'gpu0'"),
        "{note}"
    );
    assert!(!note.contains("'mem'"), "{note}");
    set_free(&state, "mem", 150);
    let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert_eq!(refusal.resource.as_deref(), Some("mem"));
}

#[test]
fn a_stale_or_failed_observation_is_charged_a_cold_load() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    observe_at(
        &state,
        vec![loaded("gemma3:12b", Some(65536), Some(rfc3339_in(290)))],
        Instant::now() - RESIDENT_FRESH_FOR - Duration::from_secs(1),
    );
    let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert!(
        refusal.detail.contains("needs 8200 MiB"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal.detail.contains("observation is 5s old"),
        "{}",
        refusal.detail
    );

    gemma_resident(&state);
    state.upstreams["up"]
        .models
        .write()
        .unwrap()
        .record_failure("model observation unavailable");
    let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert!(refusal.detail.contains("unavailable"), "{}", refusal.detail);
}

#[test]
fn an_observation_older_than_a_yield_is_charged_a_cold_load() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);
    let upstream = state.config.upstreams[0].clone();
    std::thread::sleep(Duration::from_millis(2));
    {
        let mut ledger = state.admission.lock().unwrap();
        assert!(ledger.begin_yield(&upstream));
        ledger.end_yield(&upstream);
    }
    std::thread::sleep(Duration::from_millis(2));
    // A capacity sample after the yield satisfies the ledger, but the model
    // row still describes the time before the yield unloaded it.
    set_free(&state, "gpu0", 473);
    let refusal = try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
    assert!(
        refusal.detail.contains("predates the last job or yield"),
        "{}",
        refusal.detail
    );
    gemma_resident(&state);
    try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap();
}

#[test]
fn a_model_about_to_unload_or_without_a_readable_expiry_is_charged_a_cold_load() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    for (expires_at, needle) in [
        (Some(rfc3339_in(10)), "unloads in"),
        (Some(rfc3339_in(-5)), "unloads in 0s"),
        (
            Some("in five minutes".to_owned()),
            "no readable unload time",
        ),
        (None, "no readable unload time"),
    ] {
        observe(
            &state,
            vec![loaded("gemma3:12b", Some(65536), expires_at.clone())],
        );
        let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
        assert!(refusal.detail.contains("needs 8200 MiB"), "{expires_at:?}");
        assert!(refusal.detail.contains(needle), "{}", refusal.detail);
    }
    // keep_alive = -1 ("forever") reports a far-future expiry.
    observe(
        &state,
        vec![loaded(
            "gemma3:12b",
            Some(65536),
            Some("2318-01-02T03:04:05.678-06:00".into()),
        )],
    );
    decide(&state, pinned("gemma3:12b", 65536)).unwrap();
}

#[test]
fn any_spelling_of_the_resident_model_matches_canonically() {
    let state = ollama(r#""registry.ollama.ai/library/gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    for row in ["gemma3:12b", "library/gemma3:12b"] {
        observe(
            &state,
            vec![loaded(row, Some(65536), Some(rfc3339_in(290)))],
        );
        for requested in [
            "registry.ollama.ai/library/gemma3:12b",
            "library/gemma3:12b",
            "gemma3:12b",
        ] {
            let admission = decide(&state, pinned(requested, 65536));
            assert!(admission.is_ok(), "{row} / {requested}: {admission:?}");
        }
    }
}

#[test]
fn another_resident_model_earns_no_credit_and_evictions_are_not_counted() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 473);
    observe(
        &state,
        vec![loaded(
            "qwen2.5-coder:14b",
            Some(65536),
            Some(rfc3339_in(290)),
        )],
    );
    let refusal = decide(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert!(
        refusal.detail.contains("gemma3:12b is not loaded"),
        "{}",
        refusal.detail
    );
}

#[test]
fn the_learned_prior_is_credited_the_same_way() {
    // The reviewer's second reproducer: no configured requirement, so the
    // footprint prior learned from the first load is what refuses.
    let state = state_with("ollama", "");
    set_free(&state, "gpu0", 8673);
    let upstream = state.config.upstreams[0].clone();
    let ticket = reserve(
        &state,
        &upstream,
        Some("gemma3:12b"),
        Completion::ResponseClose,
    )
    .unwrap();
    gemma_resident(&state);
    crate::lifecycle::sample_footprints(&state, "up");
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    crate::lifecycle::sample_footprints(&state, "up");
    ticket.response_complete();
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);

    let refusal = try_reserve(&state, Demand::model(Some("gemma3:12b"))).unwrap_err();
    assert!(
        refusal
            .detail
            .contains("measured model footprint needs 8200 MiB"),
        "{}",
        refusal.detail
    );
    try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap();
}

#[test]
fn the_learned_prior_is_reduced_to_the_resident_requirement() {
    let state = state_with(
        "ollama",
        r#"[upstream.models]
        "gemma3:12b" = { min_free_mib = 100, resident_min_free_mib = 600 }"#,
    );
    set_free(&state, "gpu0", 8673);
    let upstream = state.config.upstreams[0].clone();
    let ticket = reserve(
        &state,
        &upstream,
        Some("gemma3:12b"),
        Completion::ResponseClose,
    )
    .unwrap();
    gemma_resident(&state);
    crate::lifecycle::sample_footprints(&state, "up");
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    crate::lifecycle::sample_footprints(&state, "up");
    ticket.response_complete();
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);
    // The gate charges min(100, 600) = 100 and passes; the prior of 8200 is
    // replaced by min(8200, 600) = 600, which 473 free cannot meet.
    let refusal = try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap_err();
    assert!(
        refusal
            .detail
            .contains("it needs 600 MiB free on 'gpu0' (measured cold footprint 8200 MiB)"),
        "{}",
        refusal.detail
    );
    set_free(&state, "gpu0", 700);
    try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap();
}

/// Run `action` with this thread's log captured, and return what it logged.
fn logged_by(action: impl FnOnce()) -> String {
    let log = crate::test_log::capture();
    action();
    log.contents()
}

const ADMITTED_RESIDENT: &str = "admitted against a resident model's incremental requirement";

#[test]
fn live_63_a_credited_admission_is_logged_once() {
    // The shape found live: a configured requirement and a learned footprint prior
    // both credited for one request. Each used to log its own note.
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 8673);
    let upstream = state.config.upstreams[0].clone();
    let ticket = reserve(
        &state,
        &upstream,
        Some("gemma3:12b"),
        Completion::ResponseClose,
    )
    .unwrap();
    gemma_resident(&state);
    crate::lifecycle::sample_footprints(&state, "up");
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    crate::lifecycle::sample_footprints(&state, "up");
    ticket.response_complete();
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    gemma_resident(&state);

    let logged = logged_by(|| try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap());
    let lines = logged
        .lines()
        .filter(|line| line.contains(ADMITTED_RESIDENT))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{logged}");
    let line = lines[0];
    assert!(
        line.contains("model gemma3:12b is resident with context 65536"),
        "{line}"
    );
    assert!(
        line.contains("charged 0 MiB instead of 8200 MiB on 'gpu0'"),
        "{line}"
    );
    assert!(
        line.contains("the measured cold footprint of 8200 MiB was charged as 0 MiB on 'gpu0'"),
        "{line}"
    );
    assert_eq!(line.matches("is resident").count(), 1, "{line}");
}

#[test]
fn a_credit_from_the_learned_prior_alone_is_logged_once_with_its_label() {
    // No configured requirement to credit: only the prior note exists, and
    // the line still names what was proved.
    let state = state_with(
        "ollama",
        r#"[upstream.models]
        "gemma3:12b" = { min_free_mib = 100, resident_min_free_mib = 600 }"#,
    );
    set_free(&state, "gpu0", 8673);
    let upstream = state.config.upstreams[0].clone();
    let ticket = reserve(
        &state,
        &upstream,
        Some("gemma3:12b"),
        Completion::ResponseClose,
    )
    .unwrap();
    gemma_resident(&state);
    crate::lifecycle::sample_footprints(&state, "up");
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 473);
    crate::lifecycle::sample_footprints(&state, "up");
    ticket.response_complete();
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 700);
    gemma_resident(&state);

    let logged = logged_by(|| try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap());
    let lines = logged
        .lines()
        .filter(|line| line.contains(ADMITTED_RESIDENT))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{logged}");
    assert!(
        lines[0].contains("model gemma3:12b is resident with context 65536; the measured cold footprint of 8200 MiB was charged as 600 MiB on 'gpu0'"),
        "{}",
        lines[0]
    );
}

#[test]
fn a_cold_admission_logs_no_residency_line() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    set_free(&state, "gpu0", 9000);
    let logged = logged_by(|| try_reserve(&state, pinned("gemma3:12b", 65536)).unwrap());
    assert!(!logged.contains(ADMITTED_RESIDENT), "{logged}");
}

fn llama_slots(state: &AppState, busy: bool, at: Instant) {
    state.upstreams["up"]
        .models
        .write()
        .unwrap()
        .record_success_at(
            crate::lifecycle::Models {
                models: vec![vacancyd::status::Model {
                    name: None,
                    state: if busy { "running" } else { "loaded_idle" }.into(),
                    resident_bytes: None,
                    expires_at: None,
                    context_length: None,
                    digest: None,
                    default_context_length: None,
                }],
                busy,
            },
            at,
        );
}

#[test]
fn llama_cpp_single_model_is_resident_while_its_slots_are_fresh() {
    let state = state_with(
        "llamacpp",
        r#"default_min_free_mib = 8000
        [upstream.models]
        "big" = { min_free_mib = 12000, resident_min_free_mib = 200 }"#,
    );
    set_free(&state, "gpu0", 500);
    llama_slots(&state, false, Instant::now());
    // Any request, named or not, reuses the one model the server holds.
    let admission = admit(&state, &state.config.upstreams[0], None).unwrap();
    assert!(admission
        .residency
        .unwrap()
        .contains("the served model is resident; charged 0 MiB instead of 8000 MiB"));
    try_reserve(&state, Demand::model(Some("anything"))).unwrap();
    // Each finished job needs capacity and slots observed after it.
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 500);
    llama_slots(&state, false, Instant::now());
    try_reserve(&state, Demand::model(Some("big"))).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    set_free(&state, "gpu0", 150);
    llama_slots(&state, false, Instant::now());
    let refusal = try_reserve(&state, Demand::model(Some("big"))).unwrap_err();
    assert!(
        refusal.detail.contains("still needs 200 MiB"),
        "{}",
        refusal.detail
    );

    set_free(&state, "gpu0", 500);
    llama_slots(
        &state,
        false,
        Instant::now() - RESIDENT_FRESH_FOR - Duration::from_secs(1),
    );
    let refusal = admit(&state, &state.config.upstreams[0], None).unwrap_err();
    assert_eq!(refusal.reason, RefusalReason::InsufficientCapacity);
    assert!(
        refusal.detail.contains("needs 8000 MiB"),
        "{}",
        refusal.detail
    );
}

fn comfy_pool(state: &AppState, mib: Option<u64>) {
    state.upstreams["up"]
        .models
        .write()
        .unwrap()
        .record_success(crate::lifecycle::Models {
            models: vec![vacancyd::status::Model {
                name: None,
                state: "unknown".into(),
                resident_bytes: mib.map(|mib| mib * MIB),
                expires_at: None,
                context_length: None,
                digest: None,
                default_context_length: None,
            }],
            busy: false,
        });
}

#[test]
fn comfyui_allocator_memory_is_credited_on_a_single_gpu_route() {
    let state = state_with(
        "comfyui",
        r#"default_min_free_mib = 6000
        also_requires = ["mem"]"#,
    );
    set_free(&state, "gpu0", 1000);
    comfy_pool(&state, Some(6000));
    let admission = admit(&state, &state.config.upstreams[0], None).unwrap();
    assert!(admission.residency.unwrap().contains(
        "the upstream's allocator holds 6000 MiB on 'gpu0'; charged 0 MiB instead of 6000 MiB on 'gpu0'"
    ));

    comfy_pool(&state, Some(3000));
    let refusal = admit(&state, &state.config.upstreams[0], None).unwrap_err();
    assert!(
        refusal
            .detail
            .contains("still needs 3000 MiB free on 'gpu0', only 1000 MiB free"),
        "{}",
        refusal.detail
    );

    comfy_pool(&state, None);
    let refusal = admit(&state, &state.config.upstreams[0], None).unwrap_err();
    assert!(
        refusal.detail.contains("needs 6000 MiB"),
        "{}",
        refusal.detail
    );
}

#[test]
fn comfyui_allocator_memory_is_not_credited_across_two_gpus() {
    let state = state_with(
        "comfyui",
        r#"default_min_free_mib = 6000
        also_requires = ["gpu1"]"#,
    );
    set_free(&state, "gpu0", 1000);
    comfy_pool(&state, Some(8000));
    let refusal = admit(&state, &state.config.upstreams[0], None).unwrap_err();
    assert!(
        refusal.detail.contains("needs 6000 MiB"),
        "{}",
        refusal.detail
    );
    assert!(refusal.detail.contains("one GPU"), "{}", refusal.detail);
}

#[test]
fn rfc3339_reads_go_timestamps_and_rejects_everything_else() {
    assert_eq!(rfc3339_unix_ms("1970-01-01T00:00:00Z"), Some(0));
    // date -d '2026-09-23T03:45:12.123-05:00' +%s%3N
    assert_eq!(
        rfc3339_unix_ms("2026-09-23T03:45:12.123456789-05:00"),
        Some(1_790_153_112_123)
    );
    assert_eq!(
        rfc3339_unix_ms("2024-02-29T23:59:60.5+01:30"),
        Some(1_709_245_800_500)
    );
    for bad in [
        "",
        "2026-09-23",
        "2026-09-23T03:45:12",
        "2026-09-23T03:45:12+0500",
        "2026-02-30T00:00:00Z",
        "2026-13-01T00:00:00Z",
        "2026-09-23T24:00:00Z",
        "2026-09-23T03:45:12.Z",
        "0001-01-01T00:00:00Z garbage",
        "+2026-09-23T03:45:12Z",
    ] {
        assert_eq!(rfc3339_unix_ms(bad), None, "{bad}");
    }
}

/// An upstream stub that answers each connection by request path.
async fn stub(
    routes: &'static [(&'static str, &'static str)],
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let n = socket.read(&mut request).await.unwrap();
            let line = String::from_utf8_lossy(&request[..n]);
            let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
            let body = routes
                .iter()
                .find(|(route, _)| *route == path)
                .map_or("{}", |(_, body)| *body);
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });
    (format!("http://{address}"), server)
}

fn ollama_at(target: &str) -> Arc<AppState> {
    let config = toml::from_str(&format!(
        r#"
        [server]
        listen = "127.0.0.1:0"
        [owner.gpu_pressure]
        enabled = false
        [[resource]]
        id = "gpu0"
        kind = "nvidia-gpu"
        [[upstream]]
        id = "up"
        listen = "127.0.0.1:0"
        target = "{target}"
        kind = "ollama"
        resource = "gpu0"
        [upstream.models]
        "gemma3:12b" = 8200
        "#
    ))
    .unwrap();
    let state = Arc::new(AppState::new(config).unwrap());
    state.mark_presence_sampled();
    state
}

const PS_GEMMA: &str = r#"{"models":[{"name":"gemma3:12b","model":"gemma3:12b","size":8107684003,"size_vram":8107684003,"digest":"f4031aab637d","expires_at":"2318-01-02T03:04:05.678-06:00","context_length":65536}]}"#;

#[tokio::test]
async fn the_refusal_path_refreshes_a_stale_model_observation_once() {
    let (target, server) = stub(&[("/api/ps", PS_GEMMA)]).await;
    let state = ollama_at(&target);
    set_free(&state, "gpu0", 473);
    // The background observer's last row is too old to prove anything.
    observe_at(
        &state,
        vec![],
        Instant::now() - RESIDENT_FRESH_FOR - Duration::from_secs(1),
    );
    let upstream = state.config.upstreams[0].clone();
    let ticket = reserve_with_fresh_observations(
        &state,
        &upstream,
        pinned("gemma3:12b", 65536),
        Completion::ResponseClose,
        None,
    )
    .await
    .expect("a fresh /api/ps proves residency");
    ticket.response_complete();
    let row = state.upstreams["up"]
        .models
        .read()
        .unwrap()
        .view(RESIDENT_FRESH_FOR);
    assert_eq!(row.quality, ObservationQuality::Ready);
    assert_eq!(row.value.unwrap().models[0].context_length, Some(65536));
    server.abort();
}

#[tokio::test]
async fn the_refusal_path_does_not_refresh_when_the_observation_is_already_fresh() {
    // The stub is never contacted: a fresh row that proves nothing is final.
    let (target, server) = stub(&[("/api/ps", PS_GEMMA)]).await;
    let state = ollama_at(&target);
    set_free(&state, "gpu0", 473);
    observe(&state, vec![]);
    let upstream = state.config.upstreams[0].clone();
    let refusal = reserve_with_fresh_observations(
        &state,
        &upstream,
        pinned("gemma3:12b", 65536),
        Completion::ResponseClose,
        None,
    )
    .await
    .err()
    .expect("no resident model");
    assert!(
        refusal.detail.contains("gemma3:12b is not loaded"),
        "{}",
        refusal.detail
    );
    assert!(state.upstreams["up"]
        .models
        .read()
        .unwrap()
        .view(RESIDENT_FRESH_FOR)
        .value
        .unwrap()
        .models
        .is_empty());
    server.abort();
}

#[tokio::test]
async fn a_follow_up_right_after_the_loading_job_is_admitted() {
    // The live sequence: the first request loads the model and finishes; the
    // next arrives before the collector or the model observer has run again.
    let (target, server) = stub(&[("/api/ps", PS_GEMMA)]).await;
    let state = ollama_at(&target);
    set_free(&state, "gpu0", 8981);
    let upstream = state.config.upstreams[0].clone();
    let first = reserve_for(
        &state,
        &upstream,
        pinned("gemma3:12b", 65536),
        Completion::ResponseClose,
        None,
    )
    .unwrap();
    // The background observer saw the model load while the job ran.
    observe(
        &state,
        vec![loaded("gemma3:12b", Some(65536), Some(rfc3339_in(290)))],
    );
    first.response_complete();
    let sampler = {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            set_free(&state, "gpu0", 473);
        })
    };
    let ticket = reserve_with_fresh_observations(
        &state,
        &upstream,
        pinned("gemma3:12b", 65536),
        Completion::ResponseClose,
        None,
    )
    .await
    .expect("the resident model's follow-up is admitted");
    ticket.response_complete();
    sampler.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn comfyui_reports_its_allocator_only_for_a_single_device() {
    let adapter = crate::adapters::comfyui::ComfyUi;
    let http = reqwest::Client::new();
    let (one, server) = stub(&[
        ("/prompt", r#"{"exec_info":{"queue_remaining":0}}"#),
        (
            "/system_stats",
            r#"{"devices":[{"name":"cuda:0","type":"cuda","index":0,"vram_total":17095983104,"vram_free":1073741824,"torch_vram_total":6291456000,"torch_vram_free":104857600}]}"#,
        ),
    ])
    .await;
    let models = crate::plugin::adapter::Adapter::observe_models(&adapter, &http, &one)
        .unwrap()
        .await
        .unwrap();
    assert_eq!(models.models[0].resident_bytes, Some(6_291_456_000));
    server.abort();

    let (two, server) = stub(&[
        ("/prompt", r#"{"exec_info":{"queue_remaining":0}}"#),
        (
            "/system_stats",
            r#"{"devices":[{"torch_vram_total":1},{"torch_vram_total":2}]}"#,
        ),
    ])
    .await;
    let models = crate::plugin::adapter::Adapter::observe_models(&adapter, &http, &two)
        .unwrap()
        .await
        .unwrap();
    assert_eq!(models.models[0].resident_bytes, None);
    server.abort();
}

/// The field case: an installer preloads a fixed-context alias directly on
/// the upstream, so vacancyd never saw a request load it. 2447 MiB is free
/// and a cold load needs 4600 MiB; the Modelfile fixes num_ctx at 32768.
fn preloaded_helper(state: &AppState, default_context: Option<u64>) {
    observe(
        state,
        vec![vacancyd::status::Model {
            digest: Some("f00d".into()),
            default_context_length: default_context,
            ..loaded("helper:latest", Some(32768), Some(rfc3339_in(3600)))
        }],
    );
}

#[test]
fn a_model_definition_that_fixes_the_context_credits_an_unset_request() {
    let state = ollama(r#""helper:latest" = { min_free_mib = 4600, resident_min_free_mib = 300 }"#);
    set_free(&state, "gpu0", 2447);
    let v1 = asking("helper:latest", RouteFamily::OpenAi, None);
    let native = asking("helper:latest", RouteFamily::Native, None);

    preloaded_helper(&state, Some(32768));
    for demand in [v1, native] {
        let admission = decide(&state, demand).unwrap();
        let note = admission.residency.unwrap();
        assert!(
            note.contains("the num_ctx its definition sets for requests that leave it unset"),
            "{note}"
        );
        assert!(
            note.contains("charged 300 MiB instead of 4600 MiB"),
            "{note}"
        );
    }
    // An explicit num_ctx is not the definition's to vouch for.
    assert!(decide(
        &state,
        asking("helper:latest", RouteFamily::Native, Some(8192))
    )
    .is_err());

    // No definition read yet, or one that differs from what is loaded.
    for default in [None, Some(16384)] {
        preloaded_helper(&state, default);
        let refusal = decide(&state, v1).unwrap_err();
        assert!(
            refusal
                .detail
                .contains("its definition does not fix one equal to the resident context 32768"),
            "{}",
            refusal.detail
        );
    }
}

#[test]
fn a_learned_context_outranks_the_model_definition() {
    // Requests that leave num_ctx unset were seen to load 131072 (four
    // parallel slots of 32768), so a resident 32768 would be reloaded.
    let state = ollama(r#""gemma3:12b" = 8200"#);
    let unset = asking("gemma3:12b", RouteFamily::Native, None);
    load_and_observe(&state, unset, 131072);
    observe(
        &state,
        vec![vacancyd::status::Model {
            default_context_length: Some(32768),
            ..loaded("gemma3:12b", Some(32768), Some(rfc3339_in(290)))
        }],
    );
    let refusal = decide(&state, unset).unwrap_err();
    assert!(
        refusal
            .detail
            .contains("requests with an unset num_ctx loaded context 131072, but the resident context is 32768"),
        "{}",
        refusal.detail
    );
}

#[test]
fn parallel_slots_withhold_credits_that_assume_one_slot() {
    let state = ollama(r#""gemma3:12b" = 8200"#);
    // num_ctx 8192 loaded 32768: four parallel slots.
    load_and_observe(&state, pinned("gemma3:12b", 8192), 32768);
    observe(
        &state,
        vec![vacancyd::status::Model {
            default_context_length: Some(32768),
            ..loaded("gemma3:12b", Some(32768), Some(rfc3339_in(290)))
        }],
    );
    // What was learned is still credited.
    decide(&state, pinned("gemma3:12b", 8192)).unwrap();
    // Asking for the loaded context itself would load 4 x 32768.
    let refusal = decide(&state, pinned("gemma3:12b", 32768)).unwrap_err();
    assert!(
        refusal.detail.contains(
            "the upstream runs parallel slots (a request with num_ctx 8192 loaded context 32768)"
        ),
        "{}",
        refusal.detail
    );
    // So would leaving it to a definition of 32768.
    let refusal = decide(&state, asking("gemma3:12b", RouteFamily::Native, None)).unwrap_err();
    assert!(
        refusal.detail.contains("parallel slots"),
        "{}",
        refusal.detail
    );
}

const PS_HELPER: &str = r#"{"models":[{"name":"helper-qwen3.5-4b-32k:latest","model":"helper-qwen3.5-4b-32k:latest","size":4800000000,"size_vram":4800000000,"digest":"9f1a2b","expires_at":"2318-01-02T03:04:05.678-06:00","context_length":32768}]}"#;
const SHOW_HELPER: &str = r#"{"parameters":"num_ctx                        32768\nstop                           \"<|im_end|>\"\ntemperature                    0.7","modelfile":"FROM qwen3.5:4b\nPARAMETER num_ctx 32768"}"#;

#[tokio::test]
async fn ollama_reads_each_digests_definition_beside_the_observation() {
    let adapter = crate::adapters::ollama::Ollama::default();
    let http = reqwest::Client::new();
    let (target, server) = stub(&[("/api/ps", PS_HELPER), ("/api/show", SHOW_HELPER)]).await;
    let (adapter_ref, http_ref, target_ref) = (&adapter, &http, target.as_str());
    let observe = move || async move {
        crate::plugin::adapter::Adapter::observe_models(adapter_ref, http_ref, target_ref)
            .unwrap()
            .await
            .unwrap()
            .models
            .remove(0)
    };
    // The first observation starts the lookup and never waits for it.
    let first = observe().await;
    assert_eq!(first.digest.as_deref(), Some("9f1a2b"));
    assert_eq!(first.default_context_length, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = observe().await;
        if row.default_context_length == Some(32768) {
            break;
        }
        assert!(Instant::now() < deadline, "the definition was never read");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();

    // A definition without num_ctx proves nothing, and neither does an
    // upstream whose /api/show fails.
    let adapter = crate::adapters::ollama::Ollama::default();
    let (target, server) = stub(&[("/api/ps", PS_HELPER)]).await;
    for _ in 0..5 {
        let row = crate::plugin::adapter::Adapter::observe_models(&adapter, &http, &target)
            .unwrap()
            .await
            .unwrap()
            .models
            .remove(0);
        assert_eq!(row.default_context_length, None);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();
}
