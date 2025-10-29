use remote_config_proto::{ClientGetConfigsResponse, ConfigState};

#[test]
fn test_response_json_serialization() {
    // Create a response with binary data
    let response = ClientGetConfigsResponse {
        roots: vec![vec![1, 2, 3, 4, 5]],
        targets: vec![6, 7, 8, 9, 10],
        target_files: vec![],
        client_configs: vec![],
        config_status: 0,
    };

    // Serialize to JSON
    let json = serde_json::to_string_pretty(&response).unwrap();
    println!("JSON output:\n{}", json);

    // Verify roots is serialized as base64 array of strings
    assert!(
        json.contains("\"AQIDBAU=\""),
        "roots should contain base64-encoded string. Got: {}",
        json
    );
    assert!(
        !json.contains("\"roots\": [["),
        "roots should NOT be an array of integer arrays. Got: {}",
        json
    );

    // Verify targets is serialized as base64 string
    assert!(
        json.contains("\"BgcICQo=\""),
        "targets should contain base64-encoded string. Got: {}",
        json
    );
    assert!(
        !json.contains("\"targets\": ["),
        "targets should NOT be an array of integers. Got: {}",
        json
    );

    eprintln!("✓ Binary fields correctly serialized as base64!");
}

#[test]
fn test_config_state_null_apply_error() {
    // Test that ConfigState can be deserialized with null apply_error
    let json = r#"{
        "id": "asm_auto_user_instrum",
        "version": 2,
        "product": "ASM_FEATURES",
        "apply_state": 2,
        "apply_error": null
    }"#;

    let config_state: ConfigState = serde_json::from_str(json).unwrap();

    assert_eq!(config_state.id, "asm_auto_user_instrum");
    assert_eq!(config_state.version, 2);
    assert_eq!(config_state.product, "ASM_FEATURES");
    assert_eq!(config_state.apply_state, 2);
    assert_eq!(config_state.apply_error, ""); // null should be treated as empty string

    eprintln!("✓ ConfigState with null apply_error correctly deserialized!");
}
