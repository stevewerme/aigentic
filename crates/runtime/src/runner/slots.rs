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

use crate::workflow::render::trailer_model;
use crate::workflow::{SlotDecl, SlotKind, Step};

use super::forge::Forge;
use super::git::Repo;
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

/// A report's slots as the workflow declares them. Models write every
/// slot value as a string, so a `bool` slot arrives as `"false"` — which
/// a section would read as truthy — and `commits`, which the brief
/// template asks for as a JSON list, arrives as that list's text — once
/// with objects for items and prose after it — which E2 would read as no
/// subjects (both found by slice 1's acceptance run). `commits` becomes a
/// list of subject strings when every item yields one. Anything else is
/// left as written.
pub(crate) fn normalized(
    mut slots: BTreeMap<String, Value>,
    declared: &[SlotDecl],
) -> BTreeMap<String, Value> {
    for decl in declared.iter().filter(|decl| decl.kind == SlotKind::Bool) {
        if let Some(Value::String(text)) = slots.get(&decl.name) {
            let flag = match text.trim().to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            if let Some(flag) = flag {
                slots.insert(decl.name.clone(), Value::Bool(flag));
            }
        }
    }
    if let Some(subjects) = slots.get("commits").and_then(commit_subjects) {
        slots.insert(
            "commits".into(),
            Value::Array(subjects.into_iter().map(Value::String).collect()),
        );
    }
    slots
}

/// The subjects a `commits` slot names, however a model wrote it: a list,
/// or the text of one with anything after it (the #29 brief added a
/// parenthetical), whose items are subjects or `{"subject": …}` objects.
/// `None` unless every item yields a subject: a slot only partly
/// understood is left as written rather than half-read.
fn commit_subjects(value: &Value) -> Option<Vec<String>> {
    let parsed;
    let items = match value {
        Value::Array(items) => items,
        Value::String(text) => {
            parsed = serde_json::Deserializer::from_str(text.trim())
                .into_iter::<Value>()
                .next()?
                .ok()?;
            parsed.as_array()?
        }
        _ => return None,
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(subject) => Some(subject.clone()),
            Value::Object(fields) => fields
                .get("subject")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The slot map (commit 3 moves this into `runner/slots.rs` and adds the
// template line)
// ---------------------------------------------------------------------------

impl<F: Forge, H: RunnerHost, R: Repo> Runner<F, H, R> {
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
            if let Some((mut report, _)) = self.report_of(&id, record.attempt)? {
                report.slots = report
                    .slots
                    .map(|slots| normalized(slots, &self.workflow.workflow.slots));
                reports.push(report);
            }
        }
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decl(name: &str, kind: SlotKind) -> SlotDecl {
        SlotDecl {
            name: name.into(),
            kind,
            required: false,
            filled_by: "brief".into(),
        }
    }

    fn slots(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn a_bool_slot_written_as_text_is_a_bool() {
        let declared = [decl("ui", SlotKind::Bool), decl("size", SlotKind::String)];
        let out = normalized(
            slots(&[("ui", json!("false")), ("size", json!("false"))]),
            &declared,
        );
        assert_eq!(out["ui"], json!(false), "a declared bool is read");
        assert_eq!(out["size"], json!("false"), "a string slot is left alone");
        let out = normalized(slots(&[("ui", json!(" True "))]), &declared);
        assert_eq!(out["ui"], json!(true));
        let out = normalized(slots(&[("ui", json!(true))]), &declared);
        assert_eq!(out["ui"], json!(true), "a real bool stays");
        let out = normalized(slots(&[("ui", json!("maybe"))]), &declared);
        assert_eq!(out["ui"], json!("maybe"), "neither word: left as written");
    }

    #[test]
    fn commits_as_the_acceptance_briefs_wrote_them_are_subjects() {
        // The two texts #29's briefs reported, verbatim.
        let first = normalized(
            slots(&[("commits", json!("[\"gitignore: .scratch/\"]"))]),
            &[],
        );
        assert_eq!(first["commits"], json!(["gitignore: .scratch/"]));
        let second = normalized(
            slots(&[(
                "commits",
                json!(
                    "[{\"subject\": \"gitignore: .scratch/\"}] (precedent for style: `gitignore: .DS_Store` on 5d18e0e)"
                ),
            )]),
            &[],
        );
        assert_eq!(second["commits"], json!(["gitignore: .scratch/"]));
        let objects = normalized(
            slots(&[("commits", json!([{"subject": "a: b"}, "c: d"]))]),
            &[],
        );
        assert_eq!(
            objects["commits"],
            json!(["a: b", "c: d"]),
            "a real list of objects too"
        );
        let numbers = normalized(slots(&[("commits", json!("[1, 2]"))]), &[]);
        assert_eq!(
            numbers["commits"],
            json!("[1, 2]"),
            "partly understood: left as written"
        );
    }

    #[test]
    fn commits_written_as_json_text_are_a_list() {
        let out = normalized(
            slots(&[("commits", json!("[\"gitignore: .scratch/\"]"))]),
            &[],
        );
        assert_eq!(out["commits"], json!(["gitignore: .scratch/"]));
        let out = normalized(slots(&[("commits", json!(["a: b"]))]), &[]);
        assert_eq!(out["commits"], json!(["a: b"]), "a real list stays");
        let out = normalized(slots(&[("commits", json!("a: b"))]), &[]);
        assert_eq!(out["commits"], json!("a: b"), "not JSON: left as written");
        let out = normalized(slots(&[("commits", json!("{\"a\": 1}"))]), &[]);
        assert_eq!(
            out["commits"],
            json!("{\"a\": 1}"),
            "not a list: left as written"
        );
    }
}
