use atom_core::types::{FunctionCall, StreamChunk};

#[test]
fn stream_tool_call_delta_accepts_null_id_and_name() {
    // mimo-v2.5 streams tool calls in two phases: first a chunk with
    // id and name set, then fragments with id=null and name=null and
    // partial arguments. Plain `#[serde(default)]` on a String field
    // does NOT cover null (default applies only when the field is
    // missing), so the fragment chunks fail to deserialize and the
    // stream_chat loop in providers.rs silently drops them.
    let chunk_json = r#"{"choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"{\"pattern\":\"version\"}"}}]}}]}"#;
    let chunk: StreamChunk = serde_json::from_str(chunk_json)
        .expect("fragment chunk with null id/name must deserialize");
    let tc = &chunk.choices[0].delta.tool_calls[0];
    assert_eq!(tc.index, 0);
    assert_eq!(tc.id, "");
    assert_eq!(tc.call_type, "function");
    assert_eq!(tc.function.name, "");
    assert_eq!(tc.function.arguments, r#"{"pattern":"version"}"#);
}

#[test]
fn function_call_accepts_null_name() {
    let fc: FunctionCall = serde_json::from_str(r#"{"name":null,"arguments":null}"#)
        .expect("FunctionCall must accept null name and arguments");
    assert_eq!(fc.name, "");
    assert_eq!(fc.arguments, "");
}

#[test]
fn stream_chunk_accepts_type_field_as_null() {
    // Some providers stream `"type": null` on continuation chunks.
    let chunk_json = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","type":null,"function":{"name":"grep","arguments":""}}]}}]}"#;
    let chunk: StreamChunk =
        serde_json::from_str(chunk_json).expect("null type field must deserialize as empty string");
    let tc = &chunk.choices[0].delta.tool_calls[0];
    assert_eq!(tc.call_type, "");
    assert_eq!(tc.id, "x");
}

#[test]
fn full_mimo_v2_5_stream_chunks_all_deserialize() {
    // The exact SSE chunks from opencode-go's MiMo-v2.5 for a one-shot
    // "find the version" prompt, captured via curl. The first chunk
    // carries the full id and name with empty arguments; subsequent
    // chunks are argument fragments with id=null and name=null. Before
    // the fix, only the first chunk survived, leaving the call with
    // arguments = "" — which is what the persisted atom session shows.
    // The test below asserts every chunk now round-trips through
    // StreamChunk deserialization; the accumulation is then a regular
    // string-concat handled by ToolCallAccumulator (covered by its
    // own tests in atom-server).
    let raw_chunks = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_3fa4a4a8e68640f7960b988f","type":"function","function":{"name":"grep","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"{\"pattern\": "}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"\""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"version|"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"Version"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"|"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"VERSION"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"\""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":"function","function":{"name":null,"arguments":"}"}}]}}]}"#,
    ];
    for (i, c) in raw_chunks.iter().enumerate() {
        let chunk: StreamChunk = serde_json::from_str(c)
            .unwrap_or_else(|e| panic!("chunk {i} must deserialize: {e}: {c}"));
        assert!(
            !chunk.choices[0].delta.tool_calls.is_empty(),
            "chunk {i} should have a tool_call delta"
        );
    }
}
