//! The Messages adapter's request dump: the shared helper writes one
//! pretty file per call, holding the body that went over the wire.

use std::io::{Read, Write};
use std::net::TcpListener;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Message, Provider, Role, ToolSpec, UserId,
};
use aigentic_providers::{Anthropic, AnthropicConfig};
use futures_util::StreamExt;

/// One stub answering `400 Bad Request` with the request body it read, so
/// the caller gets the same error a rejected request would. It reads the
/// head first and then `content-length` bytes, so a split read cannot
/// pass as a whole request.
fn stub_400() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            let read = stream.read(&mut buf).unwrap();
            raw.extend_from_slice(&buf[..read]);
            if let Some(idx) = find_head_end(&raw) {
                break idx;
            }
            assert!(read > 0, "the caller closed before sending a whole head");
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
        let mut length = 0usize;
        for line in head.lines() {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        while raw.len() < head_end + length {
            let read = stream.read(&mut buf).unwrap();
            assert!(read > 0, "the caller closed before sending a whole body");
            raw.extend_from_slice(&buf[..read]);
        }
        let body = &raw[head_end..head_end + length];
        let mut response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        let _ = stream.write_all(&response);
    });
    format!("http://{addr}")
}

fn find_head_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn context() -> (Vec<Message>, Vec<ToolSpec>) {
    (
        vec![Message {
            role: Role::User,
            author: Author::User(UserId("steve".into())),
            blocks: vec![ContentBlock::Text("hi".into())],
        }],
        vec![ToolSpec {
            name: "read_file".into(),
            description: "Read".into(),
            schema: serde_json::json!({"type": "object"}),
        }],
    )
}

/// One test only: setting the dump variable is process-wide, so no second
/// test in this file may run while it is faked.
#[tokio::test]
async fn the_messages_adapter_dumps_its_request_body() {
    const KEY: &str = "sk-canary";
    const MODEL: &str = "claude-stub";
    let dir = tempfile::tempdir().unwrap();
    let provider = Anthropic::new(AnthropicConfig::new(KEY, MODEL).with_base_url(stub_400()));

    let (messages, tools) = context();
    let request = CompletionRequest {
        messages: &messages,
        tools: &tools,
        max_output_tokens: Some(512),
    };

    let previous = std::env::var_os("AIGENTIC_DUMP_REQUESTS");
    // SAFETY: this file holds one test, so no other thread reads the dump
    // variable while it is faked.
    unsafe { std::env::set_var("AIGENTIC_DUMP_REQUESTS", dir.path()) };
    let events: Vec<_> = provider.complete(&request).collect().await;
    match previous {
        Some(previous) => unsafe { std::env::set_var("AIGENTIC_DUMP_REQUESTS", previous) },
        None => unsafe { std::env::remove_var("AIGENTIC_DUMP_REQUESTS") },
    }

    assert!(
        matches!(
            events.last().unwrap(),
            aigentic_core::ProviderEvent::Error(aigentic_core::ProviderError::Http {
                status: 400,
                ..
            })
        ),
        "the stub answered 400: {:?}",
        events.last()
    );

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap())
        .collect();
    assert_eq!(entries.len(), 1, "one call dumps one body");
    let text = std::fs::read_to_string(entries[0].path()).unwrap();
    let dumped: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        dumped,
        serde_json::to_value(provider.build_request(&request)).unwrap(),
        "the file holds the body this call built"
    );
    assert_eq!(dumped["model"], MODEL);
    assert!(!text.contains(KEY), "a dump never holds the key");
}
