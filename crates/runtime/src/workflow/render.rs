//! Parsing and rendering a template: `{{name}}` slots and
//! `{{#name}}…{{/name}}` sections, nothing else.
//!
//! The syntax is the `## Templates` comment's, verbatim: no nesting, no
//! inverted sections, no loops, no escaping. Values are inserted as they
//! are. A load parses each template once into [`Node`]s; a render walks
//! them, and it is the parse that lets the loader check every tag
//! against the workflow's declared slots before a build reaches the
//! step. A slot the map lacks is an error, never an empty string, so a
//! short slot map is a loud failure rather than a prompt with a hole in
//! it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::workflow::WorkflowError;

/// One parsed piece of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Text with no tag in it, kept as written.
    Text(String),
    /// `{{name}}`: the slot's value, inserted verbatim.
    Slot(String),
    /// `{{#name}}…{{/name}}`: kept when the slot is truthy, dropped
    /// otherwise.
    Section { name: String, body: Vec<Node> },
}

/// One template, parsed. The path is carried so a render error names the
/// file at fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    path: PathBuf,
    nodes: Vec<Node>,
}

impl Template {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The template text with the slot map applied. A slot the map lacks
    /// is an error; a list, object or null used as a scalar slot is an
    /// error.
    pub fn render(
        &self,
        slots: &BTreeMap<String, serde_json::Value>,
    ) -> Result<String, WorkflowError> {
        let mut out = String::new();
        render_nodes(&self.nodes, slots, self, &mut out)?;
        Ok(out)
    }

    /// Parse the template text. Every syntax fault (a tag without a
    /// close, a section inside a section, a stray or mismatched close,
    /// an unclosed section, a tag that is not a name) fails here, at
    /// load, naming the file.
    pub fn parse(path: &Path, text: &str) -> Result<Template, WorkflowError> {
        let mut parser = Parser {
            path,
            text,
            pos: 0,
            stack: Vec::new(),
            nodes: Vec::new(),
        };
        parser.run()?;
        Ok(Template {
            path: path.to_path_buf(),
            nodes: parser.nodes,
        })
    }
}

/// Whether a section is kept for this slot value. The rule, from the
/// `## Templates` comment: kept for a non-empty string, `true`, any
/// number, a non-empty list or a non-empty object; dropped for an absent
/// slot, `null`, `false`, `""` or `[]`.
pub fn kept(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(_)) => true,
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        Some(serde_json::Value::Object(o)) => !o.is_empty(),
    }
}

fn render_nodes(
    nodes: &[Node],
    slots: &BTreeMap<String, serde_json::Value>,
    template: &Template,
    out: &mut String,
) -> Result<(), WorkflowError> {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Slot(name) => {
                let value = slots.get(name).ok_or_else(|| WorkflowError::MissingSlot {
                    path: template.path.clone(),
                    slot: name.clone(),
                })?;
                out.push_str(&scalar(template, name, value)?);
            }
            Node::Section { name, body } => {
                if kept(slots.get(name)) {
                    render_nodes(body, slots, template, out)?;
                }
            }
        }
    }
    Ok(())
}

/// A scalar slot inserted verbatim: a string as written, a bool as
/// `true`/`false`, a number in its JSON form. A list, an object or a
/// null has no scalar form, so it is an error rather than a guess.
fn scalar(
    template: &Template,
    slot: &str,
    value: &serde_json::Value,
) -> Result<String, WorkflowError> {
    let kind = match value {
        serde_json::Value::String(s) => return Ok(s.clone()),
        serde_json::Value::Bool(b) => return Ok(b.to_string()),
        serde_json::Value::Number(n) => return Ok(n.to_string()),
        serde_json::Value::Null => "null",
        serde_json::Value::Array(_) => "a list",
        serde_json::Value::Object(_) => "an object",
    };
    Err(WorkflowError::NotScalar {
        path: template.path.clone(),
        slot: slot.to_string(),
        kind,
    })
}

struct Parser<'a> {
    path: &'a Path,
    text: &'a str,
    pos: usize,
    /// The section being filled, if any. Nesting is refused, so this
    /// holds at most one entry.
    stack: Vec<(String, Vec<Node>)>,
    nodes: Vec<Node>,
}

impl Parser<'_> {
    fn run(&mut self) -> Result<(), WorkflowError> {
        while self.pos < self.text.len() {
            let Some(open) = self.text[self.pos..].find("{{").map(|i| self.pos + i) else {
                let rest = &self.text[self.pos..];
                self.push(Node::Text(rest.to_string()));
                self.pos = self.text.len();
                break;
            };
            let Some(close) = self.text[open + 2..].find("}}").map(|i| open + 2 + i) else {
                return Err(self.syntax(open, "a `{{` with no closing `}}`"));
            };
            let tag = &self.text[open + 2..close];
            let end = close + 2;
            // A section tag alone on its line is removed whole, newline
            // included, whether the section is kept or dropped; without
            // that a dropped section leaves a blank line behind it.
            let (standalone, stop) = if tag.starts_with('#') || tag.starts_with('/') {
                self.standalone(open, end)
            } else {
                (false, end)
            };
            let before = &self.text[self.pos..open];
            self.push_text(before);
            self.pos = if standalone { stop } else { end };
            self.tag(open, tag)?;
        }
        if let Some((name, _)) = self.stack.last() {
            return Err(self.syntax(
                self.text.len(),
                &format!("section `{{{{#{name}}}}}` is never closed"),
            ));
        }
        Ok(())
    }

    /// `(is a standalone tag, where the text continues after it)`: true
    /// when the tag is the whole line, from the start of the line to its
    /// trailing newline.
    fn standalone(&self, open: usize, end: usize) -> (bool, usize) {
        let at_line_start = open == 0 || self.text.as_bytes()[open - 1] == b'\n';
        if !at_line_start {
            return (false, end);
        }
        let rest = &self.text[end..];
        if rest.starts_with("\r\n") {
            return (true, end + 2);
        }
        if rest.starts_with('\n') {
            return (true, end + 1);
        }
        (end == self.text.len(), end)
    }

    fn tag(&mut self, open: usize, tag: &str) -> Result<(), WorkflowError> {
        if let Some(name) = tag.strip_prefix('#') {
            self.check_name(open, name)?;
            if !self.stack.is_empty() {
                return Err(self.syntax(open, "a section inside a section"));
            }
            self.stack.push((name.to_string(), Vec::new()));
        } else if let Some(name) = tag.strip_prefix('/') {
            self.check_name(open, name)?;
            let Some((open_name, body)) = self.stack.pop() else {
                return Err(self.syntax(open, "a `{{/..}}` with no open section"));
            };
            if open_name != name {
                return Err(self.syntax(
                    open,
                    &format!("section `{open_name}` is closed as `{name}`"),
                ));
            }
            self.push(Node::Section {
                name: open_name,
                body,
            });
        } else {
            self.check_name(open, tag)?;
            self.push(Node::Slot(tag.to_string()));
        }
        Ok(())
    }

    fn check_name(&self, open: usize, name: &str) -> Result<(), WorkflowError> {
        let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if ok {
            return Ok(());
        }
        Err(self.syntax(open, &format!("`{{{{{name}}}}}` is not a slot name")))
    }

    fn push_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.push(Node::Text(text.to_string()));
        }
    }

    fn push(&mut self, node: Node) {
        match self.stack.last_mut() {
            Some((_, body)) => body.push(node),
            None => self.nodes.push(node),
        }
    }

    fn syntax(&self, at: usize, message: &str) -> WorkflowError {
        WorkflowError::Syntax {
            path: self.path.to_path_buf(),
            message: format!("{message} (byte {at})"),
        }
    }
}

/// The model id a trainer trailer names: a profile's `model` without the
/// provider prefix (`deepseek/deepseek-v4.1-flash` →
/// `deepseek-v4.1-flash`). The runner fills the `model` slot with it.
pub fn trailer_model(model: &str) -> &str {
    match model.rsplit_once('/') {
        Some((_, after)) => after,
        None => model,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tests::{one_step, slot, temp};
    use serde_json::{Value, json};

    fn map(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect()
    }

    /// T12 — a scalar inserted verbatim: a string as written (no
    /// escaping), a bool as `true`, a number in its JSON form.
    #[test]
    fn render_substitutes_slots() {
        let root = temp();
        let slots = format!(
            "{}{}{}",
            slot("issue", "string", "runner", true),
            slot("strict", "bool", "runner", true),
            slot("count", "string", "runner", true)
        );
        let loaded = one_step(
            root.path(),
            &slots,
            "{{issue}} {{strict}} {{count}} {{issue}}",
        );
        let text = "a<b & \"c\"";
        let out = loaded
            .render(
                "s",
                &map(&[
                    ("issue", json!(text)),
                    ("strict", json!(true)),
                    ("count", json!(7)),
                ]),
            )
            .unwrap();
        assert_eq!(out, format!("{text} true 7 {text}"));
    }

    /// T12 — a slot the map lacks is an error, never an empty string.
    #[test]
    fn render_missing_slot_is_an_error() {
        let root = temp();
        let loaded = one_step(
            root.path(),
            &slot("issue", "string", "runner", true),
            "{{issue}}\n",
        );
        assert!(matches!(
            loaded.render("s", &BTreeMap::new()),
            Err(WorkflowError::MissingSlot { slot, .. }) if slot == "issue"
        ));
    }

    /// The other half of the scalar rule: a list, an object or a null
    /// has no scalar form.
    #[test]
    fn render_non_scalar_slot_is_an_error() {
        let root = temp();
        let loaded = one_step(
            root.path(),
            &slot("issue", "string", "runner", true),
            "{{issue}}\n",
        );
        for value in [json!(["a"]), json!({"a": 1}), json!(null)] {
            assert!(matches!(
                loaded.render("s", &map(&[("issue", value)])),
                Err(WorkflowError::NotScalar { slot, .. }) if slot == "issue"
            ));
        }
    }

    /// T12 — section truthiness for every JSON value kind: kept for a
    /// non-empty string, `true`, any number, a non-empty list or object;
    /// dropped for an absent slot, `null`, `false`, `""`, `[]` and `{}`.
    #[test]
    fn render_section_truthiness() {
        let root = temp();
        let loaded = one_step(
            root.path(),
            &slot("flag", "string", "runner", false),
            "[{{#flag}}in{{/flag}}]\n",
        );
        let expect = |kept: bool| format!("[{}]\n", if kept { "in" } else { "" });
        let rows: [(Option<Value>, bool); 11] = [
            (None, false),
            (Some(json!(null)), false),
            (Some(json!(false)), false),
            (Some(json!("")), false),
            (Some(json!([])), false),
            (Some(json!({})), false),
            (Some(json!(true)), true),
            (Some(json!("x")), true),
            (Some(json!(0)), true),
            (Some(json!([1])), true),
            (Some(json!({"a": 1})), true),
        ];
        for (value, kept) in rows {
            let slots = match value {
                Some(value) => map(&[("flag", value)]),
                None => BTreeMap::new(),
            };
            assert_eq!(
                loaded.render("s", &slots).unwrap(),
                expect(kept),
                "flag = {slots:?}"
            );
        }
    }

    /// T12 — a section tag alone on its line is removed whole, newline
    /// included, kept or dropped; a tag with other text on its line
    /// keeps that text.
    #[test]
    fn render_standalone_section_tags_are_removed() {
        let root = temp();
        let flag = slot("flag", "string", "runner", false);
        let kept = map(&[("flag", json!("x"))]);

        let loaded = one_step(root.path(), &flag, "A\n{{#flag}}\nB\n{{/flag}}\nC\n");
        assert_eq!(loaded.render("s", &kept).unwrap(), "A\nB\nC\n");
        assert_eq!(loaded.render("s", &BTreeMap::new()).unwrap(), "A\nC\n");

        let inline = one_step(root.path(), &flag, "A\nx{{#flag}}B{{/flag}}y\nC\n");
        assert_eq!(inline.render("s", &kept).unwrap(), "A\nxBy\nC\n");
        assert_eq!(inline.render("s", &BTreeMap::new()).unwrap(), "A\nxy\nC\n");
    }

    /// T14 — the `model` slot is the profile's model without the
    /// provider prefix.
    #[test]
    fn trailer_model_strips_the_provider_prefix() {
        assert_eq!(
            trailer_model("deepseek/deepseek-v4.1-flash"),
            "deepseek-v4.1-flash"
        );
        assert_eq!(trailer_model("flash"), "flash");
    }
}
