use vacancyd::settings::{SettingsSnapshot, SettingsUpdate};

const SETTINGS: &str = include_str!("fixtures/settings/default.json");

#[test]
fn browser_fixture_matches_the_complete_native_wire_contract() {
    let expected: serde_json::Value = serde_json::from_str(SETTINGS).unwrap();
    let snapshot: SettingsSnapshot = serde_json::from_str(SETTINGS).unwrap();
    // Exact JSON equality catches missing fields as well as silently ignored
    // fixture fields. The browser reads this same file rather than a copy.
    assert_eq!(serde_json::to_value(&snapshot).unwrap(), expected);
    let update = SettingsUpdate::from(snapshot);
    let mut expected_update = expected;
    expected_update
        .as_object_mut()
        .unwrap()
        .remove("application");
    expected_update
        .as_object_mut()
        .unwrap()
        .remove("capabilities");
    assert_eq!(serde_json::to_value(update).unwrap(), expected_update);
}
