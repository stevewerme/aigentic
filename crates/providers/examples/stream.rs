//! Manual check against a live OpenAI-compatible server. See the README.
//!
//! cargo run -p aigentic-providers --example stream -- "What is 2+2?"

use aigentic_core::{Author, ContentBlock, Message, Provider, ProviderEvent, Role, UserId};
use aigentic_providers::{OpenAiCompat, OpenAiCompatConfig, ToolDefinition};
use futures_util::StreamExt;
use std::io::Write;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Say hello in five words.".into());
    let base_url =
        std::env::var("AIGENTIC_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080/v1".into());
    let model = std::env::var("AIGENTIC_MODEL").unwrap_or_else(|_| "default".into());
    let mut config = OpenAiCompatConfig::new(base_url, model);
    if let Ok(key) = std::env::var("AIGENTIC_API_KEY") {
        config = config.with_api_key(key);
    }

    let provider = OpenAiCompat::new(config).with_tools(vec![ToolDefinition {
        name: "get_time".into(),
        description: "Current time in the given IANA timezone".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"timezone": {"type": "string"}},
            "required": ["timezone"]
        }),
    }]);

    let context = [Message {
        role: Role::User,
        author: Author::User(UserId("steve".into())),
        blocks: vec![ContentBlock::Text(prompt)],
    }];

    let mut stream = provider.complete(&context);
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(t) => {
                print!("{t}");
                std::io::stdout().flush().ok();
            }
            other => println!("\n{other:?}"),
        }
    }
}
