//! The known keys of a config file, and the keys of a parsed file that
//! are not among them.
//!
//! Config is forward compatible the way the event log is: adding a key
//! must not refuse every older binary (issue #37). So the typed parses
//! dropped `deny_unknown_fields`, and this module says what they
//! skipped: [`Table::unknown`] walks a parsed `toml::Value` against a
//! [`Table`] and returns the dotted path of every key it does not list.
//!
//! A `Table` is a second list of the fields the structs `Deserialize`
//! into; a struct that gains a field must list it, or every file that
//! sets it warns. The tests pin that a fixture using every field is
//! clean.

/// One table of a config file: the keys it may hold, and, for the keys
/// that hold tables, what lies inside those.
///
/// `loose` marks a table whose keys are user-chosen names (`[profiles]`,
/// `[participants]`): no name in it is ever reported. A `named` table's
/// child list carries the shape a name may hold; an `open` one does not
/// care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table {
    /// The keys this table may hold, when it is not a map of names.
    pub keys: &'static [&'static str],
    /// What each key that holds a table holds.
    pub children: &'static [(&'static str, Table)],
    /// A map of user-chosen names. `Some(child)` is the shape every name
    /// shares; `open` names nothing, so no name is reported and nothing
    /// under one is judged. `None` is an ordinary table.
    pub names: Option<Option<&'static Table>>,
}

impl Table {
    /// The table every name in a map holds, when one is described.
    fn any_child(&self) -> Option<&'static Table> {
        self.names.flatten()
    }

    /// Whether this is a map of user-chosen names, whose names are never
    /// reported.
    fn is_map(&self) -> bool {
        self.names.is_some()
    }

    /// A table whose keys are all known.
    pub const fn new(keys: &'static [&'static str]) -> Self {
        Self {
            keys,
            children: &[],
            names: None,
        }
    }

    /// A table with named children: the keys listed here, and, for the
    /// ones that hold tables, what lies inside those.
    pub const fn with(
        keys: &'static [&'static str],
        children: &'static [(&'static str, Table)],
    ) -> Self {
        Self {
            keys,
            children,
            names: None,
        }
    }

    /// A map of user names to tables, each following `child`: the names
    /// are never reported, the keys inside each one are.
    pub const fn named(child: &'static Table) -> Self {
        Self {
            keys: &[],
            children: &[],
            names: Some(Some(child)),
        }
    }

    /// A map of user names to values (`[participants]`): never reported
    /// and never descended.
    pub const fn open() -> Self {
        Self {
            keys: &[],
            children: &[],
            names: Some(None),
        }
    }

    /// The table a listed key holds, when one is described here.
    fn under(&self, key: &str) -> Option<&Table> {
        self.children
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, table)| table)
    }

    /// Whether `key` is known here.
    pub fn knows(&self, key: &str) -> bool {
        self.is_map() || self.keys.contains(&key)
    }

    /// The dotted paths under `value` that this table does not know,
    /// sorted. Arrays are not descended: a repeated table answers to its
    /// own typed parse.
    pub fn unknown(&self, value: &toml::Value) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(value, "", &mut out);
        out.sort();
        out
    }

    fn walk(&self, value: &toml::Value, prefix: &str, out: &mut Vec<String>) {
        let toml::Value::Table(table) = value else {
            return;
        };
        for (key, value) in table {
            let path = join(prefix, key);
            let child = if let Some(child) = self.under(key) {
                child
            } else if self.knows(key) {
                // A key this table knows whose value is a name in a map
                // the table above describes.
                match self.any_child() {
                    Some(child) => child,
                    None => continue,
                }
            } else {
                out.push(path);
                continue;
            };
            if matches!(value, toml::Value::Table(_)) {
                child.walk(value, &path, out);
            }
        }
    }

    /// The table `dotted` names, when it names one here.
    pub fn descends(&self, dotted: &str) -> Option<&Table> {
        if dotted.is_empty() {
            return Some(self);
        }
        let mut here = self;
        for part in dotted.split('.') {
            here = match here.under(part) {
                Some(child) => child,
                // A name in a map: every name holds the same table.
                None => here.any_child()?,
            };
        }
        Some(here)
    }

    /// The listed key a dotted path was probably meant to be, when one is
    /// within edit distance 2.
    pub fn suggest(&self, dotted: &str) -> Option<String> {
        let (parent, leaf) = split(dotted);
        let keys = self.descends(parent)?.keys;
        did_you_mean(leaf, keys).map(|best| join(parent, best))
    }

    /// Every dotted path a key of this table could be meant to be, so a
    /// test can compare a fixture's keys against them.
    pub fn paths(&self, prefix: &str, out: &mut Vec<String>) {
        for key in self.keys {
            out.push(join(prefix, key));
        }
        for (name, child) in self.children {
            child.paths(&join(prefix, name), out);
        }
        if let Some(child) = self.any_child() {
            child.paths(prefix, out);
        }
    }
}

fn split(dotted: &str) -> (&str, &str) {
    match dotted.rfind('.') {
        Some(i) => (&dotted[..i], &dotted[i + 1..]),
        None => ("", dotted),
    }
}

fn join(parent: &str, leaf: &str) -> String {
    if parent.is_empty() {
        leaf.to_owned()
    } else {
        format!("{parent}.{leaf}")
    }
}

/// The candidate `answer` was probably meant to be: within edit distance
/// 2, nearest first, ties by name.
pub fn did_you_mean<'a>(answer: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        let d = distance(answer, candidate);
        if d > 2 {
            continue;
        }
        if best.is_none_or(|(bd, bc)| (d, *candidate) < (bd, bc)) {
            best = Some((d, candidate));
        }
    }
    best.map(|(_, c)| c)
}

/// Levenshtein distance: the single-character insertions, deletions and
/// substitutions that turn `a` into `b`.
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let mut row: Vec<usize> = (1..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut corner = i;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j];
            let left = if j == 0 { i + 1 } else { row[j - 1] };
            row[j] = if ca == cb {
                corner
            } else {
                1 + corner.min(left).min(above)
            };
            corner = above;
        }
    }
    row[b.len() - 1]
}
