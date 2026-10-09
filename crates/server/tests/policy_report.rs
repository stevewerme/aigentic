//! `/policy` says what the file boundary is (issue #124): the folders the
//! file tools reach without asking, and the tools it does not confine.
//! The rig builds its own temp base; nothing here touches a real config.

use std::pin::Pin;

use aigentic_runtime::Runtime;
use aigentic_runtime::aigentic_core::{
    AgentId, Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
};
use aigentic_runtime::aigentic_log::ThreadLog;
use aigentic_runtime::aigentic_policy::Policy;
use aigentic_runtime::aigentic_tools::{DEFAULT_TIMEOUT, ToolRegistry, Workdir};
use aigentic_server::reports::policy_report;
use futures_core::Stream;

/// The report never calls the model.
struct Quiet;

impl Provider for Quiet {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        unimplemented!("the report never calls the model")
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        0
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 1_048_576,
        }
    }
}

#[test]
fn t5_the_policy_report_names_the_boundary_and_the_unconfined_tools() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let project = base.join("project");
    let skills = base.join("skills/tdd");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&skills).unwrap();
    let log = ThreadLog::open(&base, ulid::Ulid::generate()).unwrap();
    let registry = ToolRegistry::builtin(Workdir::new(&project), DEFAULT_TIMEOUT);
    let policy = Policy::defaults()
        .with_root(&project, &project)
        .with_boundary_roots([skills.clone()]);
    let runtime =
        Runtime::new(Box::new(Quiet), registry, log, AgentId("worker".into())).with_policy(policy);

    let report = policy_report(&runtime);
    let lines: Vec<&str> = report.lines().take(2).collect();
    assert_eq!(
        lines[0],
        format!(
            "boundary: file tools reach {}, {}; outside, they ask (a step is denied)",
            project.display(),
            skills.display()
        )
    );
    assert_eq!(
        lines[1],
        "bash and MCP tools with paths are not confined: they reach the whole disk until the \
         sandbox (PLAN §15 item 6); in a step, and in exec, an outside path is denied"
    );
    // The rest of the report is what it was: the rules, then the bash
    // patterns, the mode and the grants.
    assert!(
        report.contains("policy rules, first match wins\n"),
        "{report}"
    );
    assert!(report.contains("bash allow patterns: "), "{report}");
    assert!(report.contains("mode manual: "), "{report}");
    assert!(report.ends_with("session grants: none"), "{report}");

    // A runtime built without a root reaches nothing, and says so.
    let dir2 = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir2.path(), ulid::Ulid::generate()).unwrap();
    let registry = ToolRegistry::builtin(Workdir::new(dir2.path()), DEFAULT_TIMEOUT);
    let bare = Runtime::new(Box::new(Quiet), registry, log, AgentId("worker".into()));
    assert!(
        policy_report(&bare).starts_with("boundary: file tools reach nothing (no root);"),
        "{}",
        policy_report(&bare)
    );
}
