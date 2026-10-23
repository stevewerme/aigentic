//! After a turn that wrote a file, the thread's side jobs refresh the
//! project brief: the brief's current state is rewritten and one
//! `brief_refreshed` is in the log once the side jobs are done.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::{Notice, Response};
use aigentic_runtime::Mode;
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, ToolCall, Usage, UserId,
};
use aigentic_runtime::aigentic_log::ThreadLog;
use aigentic_runtime::brief::project_brief_path;
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::{Mail, Mailbox, NoReports, ThreadTable};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

const BRIEF: &str = "# p\n\nA project.\n\n## Current state\n\nNothing yet.\n";
const REFRESHED: &str = "Notes are written and the next step is the review.";

/// Every model call takes the next reply, the brief refresh included:
/// with no utility profile the thread's own provider runs it.
#[derive(Clone)]
struct Script(Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>);

impl Provider for Script {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        match self.0.lock().unwrap().pop_front() {
            Some(events) => Box::pin(futures_util::stream::iter(events)),
            None => Box::pin(futures_util::stream::pending()),
        }
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
            max_context_tokens: 100_000,
        }
    }
}

impl ProviderFactory for Script {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((Box::new(self.clone()), "script".into()))
    }
}

fn usage() -> ProviderEvent {
    ProviderEvent::Usage(Usage {
        input_tokens: 1_000,
        output_tokens: 100,
        ..Usage::default()
    })
}

fn done(reason: &str) -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: reason.into(),
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

async fn ask(mailbox: &Mailbox, mail: impl FnOnce(oneshot::Sender<Response>) -> Mail) -> Response {
    let (reply, rx) = oneshot::channel();
    mailbox.send(mail(reply)).unwrap();
    rx.await.unwrap()
}

#[tokio::test]
async fn a_turn_that_wrote_a_file_refreshes_the_brief_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("p");
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        "[project]\nname = \"p\"\n[participants]\nsteve = \"admin\"\n[memory]\nenabled = false\n",
    )
    .unwrap();
    std::fs::write(project_brief_path(&root), BRIEF).unwrap();
    let threads = dir.path().join("threads");
    let script = Script(Arc::new(Mutex::new(
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "w1".into(),
                    name: "write_file".into(),
                    args: json!({"path": "notes.md", "content": "notes\n"}),
                }),
                usage(),
                done("tool_use"),
            ],
            vec![
                ProviderEvent::TextDelta("Written.".into()),
                usage(),
                done("stop"),
            ],
            vec![
                ProviderEvent::TextDelta(REFRESHED.into()),
                usage(),
                done("stop"),
            ],
        ]
        .into(),
    )));
    let config = Config::parse(
        "default_profile = \"a\"\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
    )
    .unwrap();
    let server = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: 3600,
        users: vec![UserConfig {
            name: "steve".into(),
            token_env: None,
            token: Some("t".into()),
        }],
        projects: vec![ProjectConfig {
            name: "p".into(),
            root: root.clone(),
        }],
        resume_runs: false,
    };
    let table = Arc::new(ThreadTable::new(
        Arc::new(config),
        dir.path().join("cfg"),
        Arc::new(server),
        Arc::new(script),
        Arc::new(NoReports),
        threads.clone(),
        aigentic_server::awake::detect(false),
    ));
    let id = table.create("p", steve(), true).await.unwrap().id;
    let (mailbox, _) = table.open(id).await.unwrap();
    assert_eq!(
        ask(&mailbox, |reply| Mail::SetMode {
            mode: Mode::Auto,
            reply
        })
        .await,
        Response::Ok
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (reply, opened) = oneshot::channel();
    mailbox
        .send(Mail::Subscribe {
            from_seq: 0,
            notices: tx,
            reply,
        })
        .unwrap();
    opened.await.unwrap();
    assert_eq!(
        ask(&mailbox, |reply| Mail::Post {
            author: steve(),
            blocks: vec![ContentBlock::Text("Write the notes.".into())],
            interrupt: false,
            reply,
        })
        .await,
        Response::Ok
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let notice = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the brief refreshed in time")
            .expect("the thread is open");
        if matches!(&notice, Notice::Event { event, .. } if event.kind == EventKind::BriefRefreshed)
        {
            break;
        }
    }

    let brief = std::fs::read_to_string(project_brief_path(&root)).unwrap();
    assert!(brief.contains(REFRESHED), "{brief}");
    assert!(!brief.contains("Nothing yet."), "{brief}");
    let refreshed = ThreadLog::open(&threads, id)
        .unwrap()
        .read_all()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::BriefRefreshed)
        .count();
    assert_eq!(refreshed, 1);
}
