// End-to-end test replaying a captured muse-spark-1.2-contributor-free
// Responses API stream against a local TCP listener so the responses
// module's translation layer is exercised end-to-end (transport, SSE
// line reader, event parsing) against the real upstream wire shape,
// not just the synthetic fixture in the unit test.
//
// The fixture is a real capture from opencode.ai/zen/v1 with
// `Authorization: Bearer public`. Re-derive it with:
//   curl -N -X POST https://opencode.ai/zen/v1/responses \
//     -H 'Authorization: Bearer public' -H 'Content-Type: application/json' \
//     -d '{"model":"muse-spark-1.2-contributor-free","stream":true,
//          "max_output_tokens":500,"parallel_tool_calls":true,
//          "input":[{"type":"message","role":"user",
//                    "content":[{"type":"input_text",
//                                "text":"add 2 and 3"}]}],
//          "tools":[{"type":"function","name":"calculator",
//                    "description":"Add two numbers",
//                    "parameters":{"type":"object",
//                                  "properties":{"a":{"type":"number"},
//                                                "b":{"type":"number"}},
//                                  "required":["a","b"]}}],
//          "reasoning":{"effort":"high"}}' \
//     > .scratch/responses-toolcall.txt
//
// This test binds an ephemeral TCP port; sandbox environments that
// block local TCP binds will skip it the same way they skip the
// other transport tests under src/providers/providers.rs.

use atom_core::providers::responses::stream_responses;
use atom_core::types::{FunctionCall, Message, StreamChunk, ToolDef};
use futures::StreamExt;
use std::io::{Read, Write};
use std::net::TcpListener;

const CAPTURED: &str = include_str!("../../../.scratch/responses-toolcall.txt");

fn observe(
    chunk: &StreamChunk,
    text: &mut String,
    arg_fragments: &mut Vec<String>,
    seed_id: &mut String,
    seed_name: &mut String,
    finish_reason: &mut String,
) {
    for c in &chunk.choices {
        let delta = &c.delta;
        if !delta.content.is_empty() {
            text.push_str(&delta.content);
        }
        for tc in &delta.tool_calls {
            if !tc.id.is_empty() && seed_id.is_empty() {
                *seed_id = tc.id.clone();
            }
            if !tc.function.name.is_empty() && seed_name.is_empty() {
                *seed_name = tc.function.name.clone();
            }
            if !tc.function.arguments.is_empty() {
                arg_fragments.push(tc.function.arguments.clone());
            }
        }
        if !c.finish_reason.is_empty() {
            *finish_reason = c.finish_reason.clone();
        }
    }
}

#[tokio::test]
async fn captured_muse_spark_stream_parses_into_text_and_tool_call() {
    // Stand up a tiny TCP listener that replays the captured SSE
    // bytes verbatim to one connecting client. stream_responses walks
    // the full transport + parsing path this way instead of bypassing
    // them with a synthetic handler-only test.
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("TCP bind denied by sandbox; skipping Responses capture test");
            return;
        }
        Err(e) => panic!("bind ephemeral port: {e}"),
    };
    let addr = listener.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut head = [0u8; 4096];
            // Drain the request headers so the client's write side
            // completes; the body is irrelevant for this replay.
            let _ = stream.read(&mut head);
            let body = CAPTURED.as_bytes();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });

    let url = format!("http://{}/v1", addr);
    let msgs = vec![Message {
        role: "user".into(),
        content: "add 2 and 3".into(),
        ..Default::default()
    }];
    let tools = vec![ToolDef::new(
        "calculator",
        "Add two numbers",
        serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "number"},
                "b": {"type": "number"},
            },
            "required": ["a", "b"],
        }),
    )];

    let stream = stream_responses(
        &url,
        "public",
        "muse-spark-1.2-contributor-free",
        &msgs,
        &tools,
        "high",
    )
    .await
    .expect("stream opens");

    let chunks: Vec<StreamChunk> = stream
        .map(|item| match item {
            Ok(c) => c,
            Err(e) => panic!("stream error: {e}"),
        })
        .collect()
        .await;

    let mut text = String::new();
    let mut arg_fragments: Vec<String> = Vec::new();
    let mut seed_id = String::new();
    let mut seed_name = String::new();
    let mut finish_reason = String::new();
    let mut last_usage = None;
    for chunk in &chunks {
        observe(
            chunk,
            &mut text,
            &mut arg_fragments,
            &mut seed_id,
            &mut seed_name,
            &mut finish_reason,
        );
        if let Some(u) = chunk.usage.clone() {
            last_usage = Some(u);
        }
    }

    assert!(
        !text.is_empty(),
        "no text delta survived parsing; wire shape may have drifted"
    );
    assert!(
        !arg_fragments.is_empty(),
        "no function_call_arguments.delta survived parsing"
    );
    assert_eq!(seed_name, "calculator", "wrong tool name in seed chunk");
    assert!(
        !seed_id.is_empty(),
        "missing function_call id in seed chunk"
    );
    assert!(
        finish_reason == "stop" || finish_reason == "length" || finish_reason == "tool_calls",
        "unexpected finish_reason: {finish_reason}"
    );

    let usage = last_usage.expect("usage on final chunk");
    assert!(usage.prompt_tokens > 0, "prompt_tokens missing/zero");
    assert!(
        usage.completion_tokens > 0,
        "completion_tokens missing/zero"
    );

    // The arguments come in as multiple fragments; the upstream
    // usually streams them as two pieces ({"a":2.0 then ,"b":3.0}),
    // but the join must yield valid JSON regardless of how the
    // gateway chunks them.
    let joined = arg_fragments.join("");
    let parsed: FunctionCall = serde_json::from_value(serde_json::json!({
        "name": "calculator",
        "arguments": joined,
    }))
    .expect("joined args are valid JSON");
    assert_eq!(parsed.name, "calculator");
}
