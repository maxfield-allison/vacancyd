use vacancyd::status::{decode, DecodeError, Quality, MAX_STATUS_BYTES};

const LENDING: &[u8] = include_bytes!("fixtures/status/lending.json");

#[test]
fn every_ui_state_uses_the_same_contract() {
    let fixtures: &[(&[u8], bool)] = &[
        (LENDING, true),
        (include_bytes!("fixtures/status/owner-veto.json"), false),
        (include_bytes!("fixtures/status/disabled.json"), false),
        (include_bytes!("fixtures/status/stale.json"), false),
        (include_bytes!("fixtures/status/unknown.json"), false),
        (include_bytes!("fixtures/status/calibrating.json"), false),
        (
            include_bytes!("fixtures/status/upstream-refusal.json"),
            false,
        ),
        (
            include_bytes!("fixtures/status/owner-veto-exempt.json"),
            false,
        ),
    ];
    for (bytes, vacancy) in fixtures {
        let status = decode(bytes).unwrap();
        assert_eq!(status.upstreams[0].vacancy, *vacancy);
        let serialized = serde_json::to_vec(&status).unwrap();
        assert!(decode(&serialized).is_ok());
    }
}

// The tray uses these to name the same headline as the console window, which
// tests the same fixtures in console/web/tests/dashboard.spec.mjs.
#[test]
fn calibration_is_a_normal_wait_and_bad_readings_are_named() {
    let calibrating = decode(include_bytes!("fixtures/status/calibrating.json")).unwrap();
    assert!(calibrating.calibrating_only());
    assert!(!calibrating.untrusted_evidence());
    for bytes in [
        &include_bytes!("fixtures/status/stale.json")[..],
        &include_bytes!("fixtures/status/unknown.json")[..],
    ] {
        let status = decode(bytes).unwrap();
        assert!(!status.calibrating_only());
        assert!(status.untrusted_evidence());
    }
    let lending = decode(LENDING).unwrap();
    assert!(!lending.calibrating_only());
    assert!(!lending.untrusted_evidence());

    // Any other readiness failure beside calibration is a real fault.
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/status/calibrating.json")).unwrap();
    value["readiness_failures"]
        .as_array_mut()
        .unwrap()
        .push("upstream 'inference' is not listening".into());
    assert!(!decode(&serde_json::to_vec(&value).unwrap())
        .unwrap()
        .calibrating_only());
}

#[test]
fn rejects_unknown_versions_before_decoding_the_payload() {
    assert!(matches!(
        decode(include_bytes!("fixtures/status/incompatible.json")),
        Err(DecodeError::Incompatible(2))
    ));
    assert!(matches!(
        decode(br#"{"schema_version":2}"#),
        Err(DecodeError::Incompatible(2))
    ));
    assert!(decode(br#"{"ready":true}"#).is_err());
}

#[test]
fn rejects_truncation_wrong_types_and_oversized_responses() {
    assert!(decode(&LENDING[..LENDING.len() / 2]).is_err());
    assert!(decode(br#"{"schema_version":"1"}"#).is_err());
    assert!(matches!(
        decode(&vec![b' '; MAX_STATUS_BYTES + 1]),
        Err(DecodeError::TooLarge)
    ));
}

#[test]
fn additive_fields_are_compatible_but_contradictory_verdicts_are_not() {
    let mut value: serde_json::Value = serde_json::from_slice(LENDING).unwrap();
    value["future_field"] = serde_json::json!({"anything": true});
    assert!(decode(&serde_json::to_vec(&value).unwrap()).is_ok());
    value["upstreams"][0]["vacancy"] = false.into();
    assert!(matches!(
        decode(&serde_json::to_vec(&value).unwrap()),
        Err(DecodeError::Inconsistent)
    ));
}

#[test]
fn legacy_v1_resources_default_to_required() {
    let status = decode(LENDING).unwrap();
    assert!(status.resources.iter().all(|resource| resource.required));
    assert!(status
        .resources
        .iter()
        .all(|resource| resource.required_by.is_empty()));
}

#[test]
fn a_faulty_peer_cannot_display_retained_values_as_current() {
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/status/stale.json")).unwrap();
    value["resources"][0]["free_mib"] = 12000.into();
    value["resources"][0]["pressure"]["quality"] = "unknown".into();
    value["resources"][0]["adapter_mapping"]["quality"] = "stale".into();
    let status = decode(&serde_json::to_vec(&value).unwrap()).unwrap();
    let resource = &status.resources[0];
    assert_eq!(resource.capacity.quality, Quality::Stale);
    assert_eq!(resource.free_mib, None);
    assert_eq!(resource.pressure.as_ref().unwrap().non_lender_pct, None);
    assert_eq!(resource.adapter_mapping.as_ref().unwrap().pci, None);
    assert!(!resource.lendable);
}

#[test]
fn contradictory_vacancy_never_reaches_the_renderer() {
    for field in ["lending_enabled", "owner_veto", "capacity"] {
        let mut value: serde_json::Value = serde_json::from_slice(LENDING).unwrap();
        match field {
            "lending_enabled" => value[field] = false.into(),
            "owner_veto" => value[field]["active"] = true.into(),
            _ => value["resources"][0][field]["quality"] = "stale".into(),
        }
        assert!(matches!(
            decode(&serde_json::to_vec(&value).unwrap()),
            Err(DecodeError::Inconsistent)
        ));
    }
}

#[test]
fn stale_target_health_cannot_retain_a_current_http_status() {
    for quality in ["unknown", "stale", "ready"] {
        let mut value: serde_json::Value = serde_json::from_slice(LENDING).unwrap();
        value["upstreams"][0]["health"] = serde_json::json!({
            "quality": quality, "age_s": 40, "last_error": null, "http_status": 200
        });
        let status = decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        let health = status.upstreams[0].health.as_ref().unwrap();
        assert_eq!(
            health.http_status,
            if quality == "ready" { Some(200) } else { None }
        );
    }
}

#[test]
fn held_work_cannot_be_reported_as_vacancy_and_stale_models_are_erased() {
    let mut value: serde_json::Value = serde_json::from_slice(LENDING).unwrap();
    value["upstreams"][0]["model_observation"] =
        serde_json::json!({"quality":"stale", "age_s":50, "last_error":null});
    value["upstreams"][0]["models"] = serde_json::json!([{"name":"old", "state":"loaded_idle", "resident_bytes":1234, "expires_at":null}]);
    assert!(decode(&serde_json::to_vec(&value).unwrap())
        .unwrap()
        .upstreams[0]
        .models
        .is_empty());
    let resource = value["upstreams"][0]["resource"].clone();
    value["work"] = serde_json::json!([{"id":1,"upstream":"test","model":null,"resources":[resource],"age_s":10,"completion":"unknown","phase":"held","tracking_id":null}]);
    assert!(matches!(
        decode(&serde_json::to_vec(&value).unwrap()),
        Err(DecodeError::Inconsistent)
    ));
}

const EXEMPT: &[u8] = include_bytes!("fixtures/status/owner-veto-exempt.json");

#[test]
fn an_exempt_upstream_may_lend_through_the_owner_veto() {
    let status = decode(EXEMPT).unwrap();
    assert!(status.owner_veto.active);
    assert_eq!(status.owner_veto.exempt_upstreams, ["chatterbox"]);
    assert_eq!(status.upstreams[0].owner_scope, "host");
    assert!(!status.upstreams[0].veto_exempt());
    assert!(status.upstreams[1].veto_exempt());
    assert!(status.upstreams[1].vacancy);
    assert_eq!(status.exempt_serving(), ["chatterbox"]);
}

#[test]
fn held_work_counts_as_exempt_lending_without_a_vacancy() {
    let mut value: serde_json::Value = serde_json::from_slice(EXEMPT).unwrap();
    value["upstreams"][1]["vacancy"] = false.into();
    value["upstreams"][1]["refusal"] = serde_json::json!({
        "reason": "resource_busy", "detail": "a job still owns this resource", "retry_after_s": 2
    });
    assert_eq!(
        decode(&serde_json::to_vec(&value).unwrap())
            .unwrap()
            .exempt_serving(),
        Vec::<&str>::new()
    );
    value["work"] = serde_json::json!([{"id":1,"upstream":"chatterbox","model":null,"resources":["gpu1"],"age_s":3,"completion":"response_close","phase":"forwarding","tracking_id":null}]);
    assert_eq!(
        decode(&serde_json::to_vec(&value).unwrap())
            .unwrap()
            .exempt_serving(),
        ["chatterbox"]
    );
}

#[test]
fn only_a_resource_scoped_upstream_may_report_vacancy_under_the_veto() {
    // A host-scoped upstream with a vacancy while the owner is present.
    let mut value: serde_json::Value = serde_json::from_slice(EXEMPT).unwrap();
    value["upstreams"][0]["vacancy"] = true.into();
    value["upstreams"][0]["refusal"] = serde_json::Value::Null;
    assert!(matches!(
        decode(&serde_json::to_vec(&value).unwrap()),
        Err(DecodeError::Inconsistent)
    ));
    // A scope this client does not know is not an exemption.
    let mut value: serde_json::Value = serde_json::from_slice(EXEMPT).unwrap();
    value["upstreams"][1]["owner_scope"] = "gpu".into();
    value["owner_veto"]["exempt_upstreams"] = serde_json::json!([]);
    assert!(matches!(
        decode(&serde_json::to_vec(&value).unwrap()),
        Err(DecodeError::Inconsistent)
    ));
    // Omitting the scope means host, as from a daemon that predates it.
    let mut value: serde_json::Value = serde_json::from_slice(EXEMPT).unwrap();
    value["upstreams"][1]
        .as_object_mut()
        .unwrap()
        .remove("owner_scope");
    value["owner_veto"]["exempt_upstreams"] = serde_json::json!([]);
    assert!(matches!(
        decode(&serde_json::to_vec(&value).unwrap()),
        Err(DecodeError::Inconsistent)
    ));
}

#[test]
fn the_exempt_list_must_match_the_upstream_scopes() {
    for exempt in [
        serde_json::json!([]),
        serde_json::json!(["chatterbox", "inference"]),
        serde_json::json!(["chatterbox", "chatterbox"]),
        serde_json::json!(["someone-else"]),
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(EXEMPT).unwrap();
        value["owner_veto"]["exempt_upstreams"] = exempt.clone();
        assert!(
            matches!(
                decode(&serde_json::to_vec(&value).unwrap()),
                Err(DecodeError::Inconsistent)
            ),
            "{exempt}"
        );
    }
}

#[test]
fn a_status_without_owner_scopes_reads_as_all_host_scoped() {
    let status = decode(include_bytes!("fixtures/status/owner-veto.json")).unwrap();
    assert!(status.owner_veto.exempt_upstreams.is_empty());
    assert_eq!(status.upstreams[0].owner_scope, "host");
    assert!(status.exempt_serving().is_empty());
    let serialized = serde_json::to_value(&status).unwrap();
    assert_eq!(serialized["upstreams"][0]["owner_scope"], "host");
    assert_eq!(
        serialized["owner_veto"]["exempt_upstreams"],
        serde_json::json!([])
    );
}

#[test]
fn learned_pressure_fields_are_published_ready_and_withheld_otherwise() {
    let status = decode(LENDING).unwrap();
    let pressure = status.resources[0].pressure.as_ref().unwrap();
    assert_eq!(pressure.target_pct, Some(25.0));
    assert_eq!(pressure.learned_samples, Some(8640));
    assert_eq!(pressure.learned_s, Some(43200));

    // A faulty peer's retained values are withheld like the others.
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/status/stale.json")).unwrap();
    let pressure = &mut value["resources"][0]["pressure"];
    pressure["quality"] = "unknown".into();
    pressure["target_pct"] = 40.0.into();
    pressure["learned_samples"] = 8640.into();
    pressure["learned_s"] = 43200.into();
    let status = decode(&serde_json::to_vec(&value).unwrap()).unwrap();
    let pressure = status.resources[0].pressure.as_ref().unwrap();
    assert_eq!(pressure.target_pct, None);
    assert_eq!(pressure.learned_samples, None);
    assert_eq!(pressure.learned_s, None);
}
