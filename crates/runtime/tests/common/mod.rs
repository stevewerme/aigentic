//! Shared test doubles: a scripted provider and a recording echo tool.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, BoxFuture, Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
    RiskClass, Tool, ToolCall, ToolError, ToolOutput, UserId,
};
use aigentic_log::ThreadLog;
use aigentic_runtime::{Runtime, audit_tool_results};
use aigentic_tools::ToolRegistry;
use futures_core::Stream;
use serde_json::json;

/// Every request's messages, as the provider saw them.
pub type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// Replays scripted responses in order and records every request's messages.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Vec<ProviderEvent>>>,
    seen: Seen,
}

pub fn scripted(script: Vec<Vec<ProviderEvent>>) -> (Box<dyn Provider>, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let p = ScriptedProvider {
        script: Mutex::new(script.into()),
        seen: seen.clone(),
    };
    (Box::new(p), seen)
}

impl Provider for ScriptedProvider {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let events = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        Box::pin(futures_util::stream::iter(events))
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        7
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 1000,
        }
    }
}

/// Echoes its `msg` argument and records every call.
pub struct EchoTool(pub Arc<Mutex<Vec<serde_json::Value>>>);

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Safe
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        self.0.lock().unwrap().push(args.clone());
        Box::pin(async move {
            match args["msg"].as_str() {
                Some(m) => Ok(ToolOutput {
                    content: format!("echo: {m}"),
                    is_error: false,
                }),
                None => Err(ToolError::InvalidArgs("msg missing".into())),
            }
        })
    }
}

pub fn call(id: &str, msg: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "echo".into(),
        args: json!({"msg": msg}),
    }
}

pub fn done(reason: &str) -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: reason.into(),
    }
}

pub fn usage(i: u64, o: u64) -> ProviderEvent {
    ProviderEvent::Usage(aigentic_core::Usage {
        input_tokens: i,
        output_tokens: o,
        ..Default::default()
    })
}

pub struct Harness {
    pub runtime: Runtime,
    pub seen: Seen,
    pub calls: Arc<Mutex<Vec<serde_json::Value>>>,
    pub dir: tempfile::TempDir,
}

/// The audit behind done-when 2 runs over every log a harness produced:
/// no `tool_result` without a policy record, or the test fails at drop.
impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let events = self.runtime.log().read_all().expect("log reads back");
        let missing = audit_tool_results(&events);
        assert!(
            missing.is_empty(),
            "tool_result events without a policy record at seqs {missing:?}"
        );
    }
}

pub fn harness(script: Vec<Vec<ProviderEvent>>, instructions: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    harness_with_log(script, instructions, dir, log)
}

/// A harness over a log the test has already written to.
pub fn harness_with_log(
    script: Vec<Vec<ProviderEvent>>,
    instructions: Option<&str>,
    dir: tempfile::TempDir,
    log: ThreadLog,
) -> Harness {
    let (provider, seen) = scripted(script);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: ToolRegistry = vec![Box::new(EchoTool(calls.clone())) as Box<dyn Tool>].into();
    let runtime = Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_instructions(instructions.map(str::to_owned));
    Harness {
        runtime,
        seen,
        calls,
        dir,
    }
}

pub fn steve() -> Author {
    Author::User(UserId("steve".into()))
}
