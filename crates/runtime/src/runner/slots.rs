//! The slot map every step's template is rendered with.
//!
//! A step's prompt is built from two sources, in this order: every step
//! before it in the workflow's order contributes its last report (the
//! report's `slots` map, its `planned_tests` rendered to one line per
//! test, its `commits` rendered to one subject per line), and then the
//! runner adds its own slots, which win. A slot a template wants and the
//! map lacks is a render error, and the runner escalates `render_failed`
//! before any child exists.

use std::collections::BTreeMap;

use aigentic_log::{PlannedTest, StepReport};
use serde_json::Value;

use crate::workflow::Step;
use crate::workflow::render::trailer_model;

use super::forge::Forge;
use super::host::RunnerHost;
use super::{Runner, RunnerError};

/// `T1 — what — derivation` per line, the shape the implementer's template
/// shows the model (#57's T10). The runner renders it, so no template ever
/// meets a list.
fn planned_tests_text(tests: &[PlannedTest]) -> String {
    tests
        .iter()
        .map(|test| format!("{} — {} — {}", test.id, test.what, test.derivation))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One commit subject per line, when every item is a string.
fn commits_text(items: &[Value]) -> Option<String> {
    items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .map(|subjects| subjects.join("\n"))
}

// ---------------------------------------------------------------------------
// The slot map (commit 3 moves this into `runner/slots.rs` and adds the
// template line)
// ---------------------------------------------------------------------------

impl<F: Forge, H: RunnerHost> Runner<F, H> {
    /// The slot map one step's template is rendered with: every earlier
    /// step's report, then the runner's own slots, which win.
    pub fn slot_map(&self, step: &Step) -> Result<BTreeMap<String, Value>, RunnerError> {
        let mut map: BTreeMap<String, Value> = BTreeMap::new();
        for report in self.earlier_reports(step)? {
            if let Some(slots) = report.slots {
                map.extend(slots);
            }
            // Typed report fields render to text: a list used as `{{name}}`
            // is an error, so the runner does it here.
            if let Some(tests) = report.planned_tests {
                map.insert(
                    "planned_tests".into(),
                    Value::String(planned_tests_text(&tests)),
                );
            }
            if let Some(Value::Array(items)) = map.get("commits").cloned()
                && let Some(text) = commits_text(&items)
            {
                map.insert("commits".into(), Value::String(text));
            }
        }
        map.extend(self.runner_slots(step)?);
        Ok(map)
    }

    /// What the runner fills itself, in the workflow's declaration order.
    pub fn runner_slots(&self, step: &Step) -> Result<BTreeMap<String, Value>, RunnerError> {
        let run = self.run()?;
        let budget = &self.workflow.workflow.budget;
        let model = trailer_model(&self.host.model_of(&step.profile)?).to_owned();
        // The gate's log path: where the child appends its own gate runs.
        let gate_log = std::env::temp_dir()
            .join(format!("aigentic-gate-{}.log", run.issue))
            .display()
            .to_string();
        Ok(BTreeMap::from([
            ("issue".to_owned(), Value::String(run.issue.to_string())),
            (
                "title".to_owned(),
                Value::String(self.forge.issue(run.issue)?.title),
            ),
            ("model".to_owned(), Value::String(model)),
            ("gate_log".to_owned(), Value::String(gate_log)),
            (
                "budget_trivial".to_owned(),
                Value::String(budget.trivial.to_string()),
            ),
            (
                "budget_full".to_owned(),
                Value::String(budget.full.to_string()),
            ),
            (
                "max_raise".to_owned(),
                Value::String(budget.max_raise.to_string()),
            ),
        ]))
    }

    /// What the steps before `step` reported, in the workflow's order, read
    /// from their children's logs.
    pub fn earlier_reports(&self, step: &Step) -> Result<Vec<StepReport>, RunnerError> {
        let state = self.state()?;
        let before: Vec<String> = self
            .workflow
            .workflow
            .steps
            .iter()
            .take_while(|candidate| candidate.id != step.id)
            .map(|candidate| candidate.id.clone())
            .collect();
        let mut reports = Vec::new();
        for id in before {
            let Some(record) = state
                .steps
                .iter()
                .rev()
                .find(|record| record.step == id && record.reported_event.is_some())
            else {
                continue;
            };
            if let Some((report, _)) = self.report_of(&id, record.attempt)? {
                reports.push(report);
            }
        }
        Ok(reports)
    }
}
