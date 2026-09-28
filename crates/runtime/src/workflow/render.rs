//! Parsing a template: `{{name}}` slots and `{{#name}}…{{/name}}`
//! sections, nothing else.
//!
//! The syntax is the `## Templates` comment's, verbatim: no nesting, no
//! inverted sections, no loops, no escaping. A load parses each template
//! once into [`Node`]s, which is what lets the loader check every tag
//! against the workflow's declared slots before a build ever reaches the
//! step.

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

/// One template, parsed. The path is carried so an error names the file
/// at fault.
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
        let ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
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