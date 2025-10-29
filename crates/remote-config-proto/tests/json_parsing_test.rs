use remote_config_proto::ClientGetConfigsRequest;

#[test]
fn test_parse_dotnet_tracer_json_with_nulls() {
    // This is the exact JSON from the .NET tracer that was failing
    let json = r#"{"client":{"state":{"root_version":1,"targets_version":0,"config_states":[],"has_error":false,"error":null,"backend_client_state":null},"id":"2f8b531a-6b99-4369-ae35-8b9b6650ee7d","products":["ASM_FEATURES","ASM_DD","APM_TRACING","AGENT_CONFIG","AGENT_TASK","LIVE_DEBUGGING_SYMBOL_DB","LIVE_DEBUGGING"],"is_tracer":true,"client_tracer":{"runtime_id":"c73e2bbd-f7d9-4c5c-a036-ec9521efc436","language":"dotnet","tracer_version":"3.31.0","service":"console-test","extra_services":null,"env":"tony-di-3","app_version":null,"tags":["env:tony-di-3","tracer_version:3.31.0","host_name:COMP-YJ96HQV339","git.commit.sha:","git.repository_url:"]},"capabilities":"I8CgCPf+"},"cached_target_files":[]}"#;

    // This should parse without errors now
    let result: Result<ClientGetConfigsRequest, _> = serde_json::from_str(json);

    match &result {
        Ok(req) => {
            eprintln!("✓ JSON parsed successfully!");

            // Verify the client was parsed
            assert!(req.client.is_some());
            let client = req.client.as_ref().unwrap();

            // Verify client_tracer was parsed
            assert!(client.client_tracer.is_some());
            let tracer = client.client_tracer.as_ref().unwrap();

            // Verify null fields were converted to defaults
            assert_eq!(tracer.extra_services, Vec::<String>::new(), "extra_services should be empty vec");
            assert_eq!(tracer.app_version, "", "app_version should be empty string");

            // Verify non-null fields were parsed correctly
            assert_eq!(tracer.runtime_id, "c73e2bbd-f7d9-4c5c-a036-ec9521efc436");
            assert_eq!(tracer.language, "dotnet");
            assert_eq!(tracer.tracer_version, "3.31.0");
            assert_eq!(tracer.service, "console-test");
            assert_eq!(tracer.env, "tony-di-3");
            assert_eq!(tracer.tags.len(), 5);

            eprintln!("✓ All fields validated correctly!");
        }
        Err(e) => {
            eprintln!("✗ JSON parsing failed: {}", e);
            panic!("Failed to parse .NET tracer JSON: {}", e);
        }
    }

    assert!(result.is_ok(), "Should successfully parse .NET tracer JSON with null values");
}
