//! The same canonical context through both adapters: each replays its own
//! blobs and drops the other's, so a thread can move between backends.

use aigentic_core::{
    AgentId, Author, CompletionRequest, ContentBlock, Message, ProviderBlob, Role, UserId,
};
use aigentic_providers::anthropic::{AnthropicConfig, to_wire as anthropic_wire};
use aigentic_providers::openai_compat::{OpenAiCompat, OpenAiCompatConfig};
use serde_json::json;

fn context() -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            author: Author::User(UserId("steve".into())),
            blocks: vec![ContentBlock::Text("hi".into())],
        },
        Message {
            role: Role::Assistant,
            author: Author::Agent(AgentId("worker".into())),
            blocks: vec![
                ContentBlock::ProviderBlob(ProviderBlob {
                    provider: "anthropic".into(),
                    data: json!({"type": "thinking", "thinking": "", "signature": "SIG"}),
                }),
                ContentBlock::ProviderBlob(ProviderBlob {
                    provider: "openai_compat".into(),
                    data: json!({"reasoning_content": "REASON"}),
                }),
                ContentBlock::Text("hello".into()),
            ],
        },
        Message {
            role: Role::User,
            author: Author::User(UserId("steve".into())),
            blocks: vec![ContentBlock::Text("thanks".into())],
        },
    ]
}

#[test]
fn each_adapter_replays_only_its_own_blob() {
    let messages = context();
    let request = CompletionRequest {
        messages: &messages,
        tools: &[],
        max_output_tokens: None,
    };

    let a =
        serde_json::to_string(&anthropic_wire(&request, &AnthropicConfig::new("k", "m"))).unwrap();
    assert!(
        a.contains("SIG"),
        "anthropic replays its thinking block: {a}"
    );
    assert!(
        !a.contains("REASON"),
        "anthropic drops the openai blob: {a}"
    );

    let o = OpenAiCompat::new(OpenAiCompatConfig::new("http://x/v1", "m"));
    let o = serde_json::to_string(&o.build_request(&request)).unwrap();
    assert!(
        o.contains("REASON"),
        "openai_compat replays reasoning_content: {o}"
    );
    assert!(
        !o.contains("SIG"),
        "openai_compat drops the anthropic blob: {o}"
    );
}
