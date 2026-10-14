//! The OpenAI-compatible request dump is written before the request goes
//! out, so a call whose reply never comes still leaves its body behind:
//! that is the call a person most needs to read after the fact.

use std::io::Read;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Message, Provider, Role, ToolSpec, UserId,
};
use aigentic_providers::{OpenAiCompat, OpenAiCompatConfig};
use futures_util::StreamExt;

/// A stub that reads the whole request head, says so on `seen`, and then
/// holds the connection open without ever answering.
fn silent_stub(seen: mpsc::Sender<()>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                return;
            }
            raw.extend_from_slice(&buf[..read]);
        }
        let _ = seen.send(());
        std::thread::sleep(Duration::from_secs(60));
        drop(stream);
    });
    format!("http://{addr}")
}

/// One test only: setting the dump variable is process-wide, so no second
/// test in this file may run while it is faked.
#[tokio::test]
async fn a_call_with_no_reply_has_already_dumped_its_body() {
    let dir = tempfile::tempdir().unwrap();
    let messages = vec![Message {
        role: Role::User,
        author: Author::User(UserId("steve".into())),
        blocks: vec![ContentBlock::Text("hi".into())],
    }];
    let tools: Vec<ToolSpec> = Vec::new();
    let request = CompletionRequest {
        messages: &messages,
        tools: &tools,
        max_output_tokens: Some(512),
    };

    let previous = std::env::var_os("AIGENTIC_DUMP_REQUESTS");
    // SAFETY: this file holds one test, so no other thread reads the dump
    // variable while it is faked.
    unsafe { std::env::set_var("AIGENTIC_DUMP_REQUESTS", dir.path()) };
    let (seen_tx, seen_rx) = mpsc::channel();
    let provider = OpenAiCompat::new(OpenAiCompatConfig::new(silent_stub(seen_tx), "m"));
    let mut stream = provider.complete(&request);
    let seen = tokio::task::spawn_blocking(move || seen_rx.recv_timeout(Duration::from_secs(10)));
    tokio::select! {
        event = stream.next() => panic!("the stub never answers, yet the call ended: {event:?}"),
        seen = seen => seen.unwrap().expect("the request reached the stub"),
    }

    let dumped: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    match previous {
        Some(previous) => unsafe { std::env::set_var("AIGENTIC_DUMP_REQUESTS", previous) },
        None => unsafe { std::env::remove_var("AIGENTIC_DUMP_REQUESTS") },
    }
    assert_eq!(
        dumped.len(),
        1,
        "the body is in the directory while the call is still waiting: {dumped:?}"
    );
    let text = std::fs::read_to_string(&dumped[0]).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        serde_json::to_value(provider.build_request(&request)).unwrap()
    );
}
