//! `/jobs` (issue #79): today's builds as a table, and the one row a
//! person picks to look at a build read-only.
//!
//! The table is drawn from the rows alone, so the same text reaches a
//! terminal and a pipe.

use aigentic_api::JobRow;

use crate::stats;

/// The header row and the rows under it, one line each.
///
/// Columns: the row's number in this listing, what the build is, its
/// project, its state and its cost. The header is drawn through the same
/// widths as the rows, so the columns always line up.
pub fn table(rows: &[JobRow]) -> Vec<String> {
    let mut lines = vec![row_line("#", "what", "project", "state", "cost")];
    for (i, row) in rows.iter().enumerate() {
        lines.push(row_line(
            &(i + 1).to_string(),
            &row.what,
            &row.project,
            &state_words(row),
            &stats::money(row.cost_usd, None),
        ));
    }
    lines
}

/// One table line, at the table's column widths.
fn row_line(n: &str, what: &str, project: &str, state: &str, cost: &str) -> String {
    format!("{n:>4} {what:<16} {project:<12} {state:<24} {cost}",)
}

/// A row's state in the words a person reads: the variant's own name, with
/// the reason a stopped build stopped.
///
/// A state this daemon could not read is the same `-` the cost cell uses,
/// so an unknown cell never reads as prose.
pub fn state_words(row: &JobRow) -> String {
    match &row.state {
        Some(state) => state.words(),
        None => "-".into(),
    }
}

/// The one line a caller outside the table shows about a row: what it is,
/// where, and what it is doing. `/jobs <n>` echoes it before the run's own
/// lines, so a number picked from a listing reads back.
pub fn one_line(row: &JobRow) -> String {
    format!(
        "{} · {} · {} · {}",
        row.what,
        row.project,
        state_words(row),
        stats::money(row.cost_usd, None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_api::{JobRow, JobState};
    use ulid::Ulid;

    fn row(n: u64, state: Option<JobState>, cost: Option<f64>) -> JobRow {
        JobRow {
            id: Ulid::generate(),
            issue: Some(n),
            what: format!("build #{n}"),
            project: "vendela".into(),
            state,
            cost_usd: cost,
        }
    }

    /// T11 (issue #79): the table draws the header once and one line per
    /// row, with the row's number in this listing, what it is, its
    /// project, its state in the variant's own words and its cost in the
    /// cost cell's. A row with no state and no cost falls back to the same
    /// `-` cell rather than prose.
    #[test]
    fn the_jobs_table_draws_state_and_cost() {
        let rows = vec![
            row(64, Some(JobState::Running), Some(1.5)),
            row(
                65,
                Some(JobState::Stopped {
                    reason: "provider refused".into(),
                }),
                None,
            ),
            row(66, None, None),
        ];
        let lines = table(&rows);
        assert_eq!(lines.len(), rows.len() + 1, "{lines:#?}");
        assert_eq!(lines[0], row_line("#", "what", "project", "state", "cost"));

        let expected: Vec<String> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                row_line(
                    &(i + 1).to_string(),
                    &r.what,
                    &r.project,
                    &state_words(r),
                    &stats::money(r.cost_usd, None),
                )
            })
            .collect();
        assert_eq!(lines[1..], expected[..], "{lines:#?}");

        // The words a person reads, and the `-` cell an unknown state
        // and an unpriced cost share.
        assert_eq!(state_words(&rows[0]), "running");
        assert_eq!(state_words(&rows[1]), "stopped: provider refused");
        assert_eq!(state_words(&rows[2]), stats::money(None, None));
        assert!(lines[1].contains(&state_words(&rows[0])), "{lines:#?}");
        assert!(lines[2].contains(&state_words(&rows[1])), "{lines:#?}");
        assert!(lines[3].contains(&state_words(&rows[2])), "{lines:#?}");
        assert!(
            lines[1].contains(&stats::money(rows[0].cost_usd, None)),
            "{lines:#?}"
        );
    }
}
