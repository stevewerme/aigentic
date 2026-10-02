//! The decision fold (issue #74): read a thread's events and say, per
//! proposal, what was asked and how it was answered — the record ADR 0002
//! judges a decision kind's promotion against.
//!
//! It is not named `decisions` because the runtime already has a
//! `Decisions` registry of pending proposals, and `DecisionScope` is a
//! permission answer's scope: this module reads a finished log, those
//! hold live questions.

use std::collections::HashMap;

use aigentic_core::{Author, Event, EventKind};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::payload::{
    DecisionAnswer, DecisionAnsweredPayload, DecisionKind, DecisionProposedPayload, DecisionStage,
};

/// One proposal and, if it was answered, its answer. `answer: None` is a
/// proposal still pending at the end of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    /// The `decision_proposed` event's id — what a later review greps for.
    pub id: Ulid,
    /// When the proposal was made.
    pub at: OffsetDateTime,
    pub kind: DecisionKind,
    pub stage: DecisionStage,
    pub proposal: String,
    pub target: Option<String>,
    pub reason: String,
    pub answer: Option<RecordedAnswer>,
}

/// An answer that named a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAnswer {
    pub answer: DecisionAnswer,
    pub correction: Option<String>,
    /// Who answered: a user for a person's answer, `System` for a
    /// withdrawal.
    pub by: Author,
    pub at: OffsetDateTime,
}

/// Every proposal in a thread, in log order, plus the answers that named
/// no proposal (or a second answer to an already-answered one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionFold {
    pub records: Vec<DecisionRecord>,
    pub orphans: usize,
}

/// Fold a thread's events into its decision records (issue #74). Pure: no
/// I/O, no clock, no panic on a malformed payload.
///
/// An answer is matched to its proposal by `parent_event`, over the whole
/// thread — a proposal may be answered in a later turn or after a resume,
/// whatever lies between. An answer whose `parent_event` names no
/// proposal, a second answer to an answered proposal, and a payload that
/// does not parse are all **orphans**: counted and never guessed.
pub fn decision_records(events: &[Event]) -> DecisionFold {
    let mut records: Vec<DecisionRecord> = Vec::new();
    let mut orphans = 0usize;
    // Proposal event id -> its record, so an answer finds its proposal
    // whatever lies between the two events.
    let mut by_id: HashMap<Ulid, usize> = HashMap::new();

    for event in events {
        match event.kind {
            EventKind::DecisionProposed => {
                match serde_json::from_value::<DecisionProposedPayload>(event.payload.clone()) {
                    Ok(p) => {
                        by_id.insert(event.id, records.len());
                        records.push(DecisionRecord {
                            id: event.id,
                            at: event.created_at,
                            kind: p.kind,
                            stage: p.stage,
                            proposal: p.proposal,
                            target: p.target,
                            reason: p.reason,
                            answer: None,
                        });
                    }
                    Err(_) => orphans += 1,
                }
            }
            EventKind::DecisionAnswered => {
                match serde_json::from_value::<DecisionAnsweredPayload>(event.payload.clone()) {
                    Ok(a) => {
                        match event.parent_event.and_then(|id| by_id.get(&id).copied()) {
                            // The first answer stands; a second is an
                            // orphan.
                            Some(idx) if records[idx].answer.is_none() => {
                                records[idx].answer = Some(RecordedAnswer {
                                    answer: a.answer,
                                    correction: a.correction,
                                    by: event.author.clone(),
                                    at: event.created_at,
                                });
                            }
                            _ => orphans += 1,
                        }
                    }
                    Err(_) => orphans += 1,
                }
            }
            _ => {}
        }
    }

    DecisionFold { records, orphans }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{AgentId, UserId};
    use serde_json::json;
    use time::macros::datetime;

    fn event(seq: u64, kind: EventKind, author: Author, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 42),
            seq,
            kind,
            author,
            payload,
            parent_event: None,
            created_at: datetime!(2026-09-21 12:00:00 UTC) + time::Duration::seconds(seq as i64),
        }
    }

    fn steve() -> Author {
        Author::User(UserId("steve".into()))
    }

    fn system() -> Author {
        Author::System
    }

    fn proposed(seq: u64, kind: DecisionKind, proposal: &str) -> Event {
        event(
            seq,
            EventKind::DecisionProposed,
            Author::Agent(AgentId("worker".into())),
            serde_json::to_value(DecisionProposedPayload {
                kind,
                proposal: proposal.into(),
                target: None,
                reason: "the brief names it".into(),
                call_id: None,
                stage: DecisionStage::Ask,
            })
            .unwrap(),
        )
    }

    fn answered(seq: u64, parent: Ulid, answer: DecisionAnswer, by: Author) -> Event {
        let mut e = event(
            seq,
            EventKind::DecisionAnswered,
            by,
            serde_json::to_value(DecisionAnsweredPayload {
                answer,
                correction: None,
                note: None,
            })
            .unwrap(),
        );
        e.parent_event = Some(parent);
        e
    }

    /// T3 (issue #74): a proposal and its answer pair by `parent_event`
    /// even with an interrupted event and another turn between them.
    #[test]
    fn a_proposal_and_its_answer_pair_across_turns() {
        let p = proposed(0, DecisionKind::Project, "switch to getscale/site");
        let between = event(1, EventKind::UserMessage, steve(), json!({"blocks": []}));
        let interrupted = event(2, EventKind::Interrupted, system(), json!({}));
        let a = answered(3, p.id, DecisionAnswer::Yes, steve());
        let fold = decision_records(&[p.clone(), between, interrupted, a]);
        assert_eq!(fold.orphans, 0);
        assert_eq!(fold.records.len(), 1);
        let rec = &fold.records[0];
        assert_eq!(rec.id, p.id, "the record keeps the proposal's id");
        assert_eq!(rec.kind, DecisionKind::Project);
        let answer = rec.answer.as_ref().expect("paired");
        assert_eq!(answer.answer, DecisionAnswer::Yes);
        assert_eq!(
            answer.by,
            steve(),
            "RecordedAnswer.by is the answer's author"
        );
        assert_eq!(answer.at, datetime!(2026-09-21 12:00:03 UTC));
    }

    /// T3: an unanswered proposal is pending.
    #[test]
    fn an_unanswered_proposal_is_pending() {
        let p = proposed(0, DecisionKind::Job, "start the build for #77");
        let fold = decision_records(&[p]);
        assert_eq!(fold.orphans, 0);
        assert_eq!(fold.records.len(), 1);
        assert_eq!(fold.records[0].answer, None);
    }

    /// T3: an answer naming no proposal in the log is an orphan.
    #[test]
    fn an_answer_naming_no_proposal_is_an_orphan() {
        let stranger = Ulid::from_parts(1_700_000_000_999, 9);
        let a = answered(0, stranger, DecisionAnswer::No, steve());
        let fold = decision_records(&[a]);
        assert_eq!(fold.records.len(), 0);
        assert_eq!(fold.orphans, 1);
    }

    /// T3: a second answer is an orphan, and the first stands.
    #[test]
    fn a_second_answer_is_an_orphan_and_the_first_stands() {
        let p = proposed(0, DecisionKind::Ticket, "file it as #74");
        let first = answered(1, p.id, DecisionAnswer::Yes, steve());
        let second = answered(2, p.id, DecisionAnswer::No, steve());
        let fold = decision_records(&[p, first, second]);
        assert_eq!(fold.records.len(), 1);
        assert_eq!(fold.orphans, 1);
        assert_eq!(
            fold.records[0].answer.as_ref().unwrap().answer,
            DecisionAnswer::Yes
        );
    }

    /// T3: a payload that doesn't parse is an orphan, with no panic.
    #[test]
    fn an_unparseable_payload_is_an_orphan() {
        let bad_proposal = event(0, EventKind::DecisionProposed, system(), json!({"kind": 5}));
        let bad_answer = event(
            1,
            EventKind::DecisionAnswered,
            system(),
            json!({"answer": "maybe"}),
        );
        let fold = decision_records(&[bad_proposal, bad_answer]);
        assert_eq!(fold.records.len(), 0);
        assert_eq!(fold.orphans, 2);
    }

    /// T3: a `withdrawn` written by the system is recorded like any
    /// answer, authored by the system.
    #[test]
    fn a_withdrawal_is_recorded_with_its_system_author() {
        let p = proposed(0, DecisionKind::Knowledge, "remember the preference");
        let a = answered(1, p.id, DecisionAnswer::Withdrawn, system());
        let fold = decision_records(&[p, a]);
        let answer = fold.records[0].answer.as_ref().unwrap();
        assert_eq!(answer.answer, DecisionAnswer::Withdrawn);
        assert_eq!(answer.by, Author::System);
    }
}
