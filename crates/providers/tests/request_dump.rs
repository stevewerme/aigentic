//! The OpenAI-compatible request dump through `complete`, over a real
//! server: a 400 leaves the body that earned it in the directory a person
//! named, and an empty value writes nothing at all.

use std::io::{Read, Write};
use std::net::TcpListener;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Message, Provider, ProviderError, ProviderEvent, Role,
    ToolSpec, UserId,
};
use aigentic_providers::{OpenAiCompat, OpenAiCompatConfig};
use futures_util::StreamExt;

/// One stub answering `400 Bad Request` with the request body it read, so
/// the caller sees the same error a rejected request produces. It reads
/// the head first and then exactly the bytes `content-length` promises, so
/// a split read cannot pass as a whole request.
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

fn provider(base_url: &str, key: &str) -> OpenAiCompat {
    OpenAiCompat::new(OpenAiCompatConfig::new(base_url, "m").with_api_key(key))
}

/// One test only: setting the dump variable is process-wide, so no second
/// test in this file may run while it is faked.
#[tokio::test]
async fn a_400_call_dumps_the_body_it_sent_and_unset_writes_nothing() {
    const KEY: &str = "sk-canary-160";
    let dir = tempfile::tempdir().unwrap();
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
    let first = provider(&stub_400(), KEY);
    let events: Vec<ProviderEvent> = first.complete(&request).collect().await;
    let Some(ProviderEvent::Error(ProviderError::Http { status, body })) = events.last() else {
        panic!("the stub answered 400: {events:?}");
    };
    assert_eq!(*status, 400);

    // One attempt, so one `with_attempts` prefix stands before the raw
    // body: parsing the whole thing as JSON would fail.
    let sent = serde_json::to_string(&first.build_request(&request)).unwrap();
    assert!(
        body.ends_with(&sent),
        "the error body ends with what the wire carried: {body}"
    );

    let paths = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    };
    let dumped = paths();
    assert_eq!(dumped.len(), 1, "{dumped:?}");
    assert_eq!(
        dumped[0].extension().and_then(|e| e.to_str()),
        Some("json"),
        "{dumped:?}"
    );
    let text = std::fs::read_to_string(&dumped[0]).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        serde_json::to_value(first.build_request(&request)).unwrap(),
        "the file holds the same JSON value the wire carried"
    );
    assert!(!text.contains(KEY), "a dump never holds the key");

    // An empty value names no directory, so the second call writes
    // nothing: without this, the name would join onto "" and land in the
    // process's working directory.
    unsafe { std::env::set_var("AIGENTIC_DUMP_REQUESTS", "") };
    let second = provider(&stub_400(), KEY);
    let events: Vec<ProviderEvent> = second.complete(&request).collect().await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Error(_))),
        "{events:?}"
    );
    match previous {
        Some(previous) => unsafe { std::env::set_var("AIGENTIC_DUMP_REQUESTS", previous) },
        None => unsafe { std::env::remove_var("AIGENTIC_DUMP_REQUESTS") },
    }

    let dumped = paths();
    assert_eq!(dumped.len(), 1, "an empty value writes nothing: {dumped:?}");
}
