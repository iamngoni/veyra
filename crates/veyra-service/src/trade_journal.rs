//! Ticket-linking shared by the assistant's position journal and the
//! `/trades` reporting route.
//!
//! Both need the same evidence for one venue ticket: the entry decision that
//! opened it (a `proposal_evaluated` row whose `command_id` matches the
//! `open_order` completion reporting this ticket), any recorded stop moves or
//! closes, and the terminal acknowledgements around them. [`ticket_episode`]
//! runs the two-step audit query that finds them all — rows tagged with the
//! ticket directly, plus every row sharing a command id one of those rows
//! names — so a change to the linking rule lands once for both callers.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use crate::audit::{AuditError, AuditKind, AuditQuery, AuditRow, AuditTrail, parse_trail_time};
use crate::broker::CommandId;

/// Event kinds that can mention one position or one of its commands.
pub(crate) const STORY_KINDS: [AuditKind; 5] = [
    AuditKind::ProposalEvaluated,
    AuditKind::PositionClosed,
    AuditKind::CommandQueued,
    AuditKind::CommandCompleted,
    AuditKind::CommandFailed,
];

/// Rows one ticket-episode query step may read.
pub(crate) const STORY_ROWS: u32 = 120;

/// One ticket's linked audit rows, oldest first, and the `open_order`
/// command ids whose completion reported this ticket (normally at most one;
/// a list survives a hand-edited or replayed trail without panicking).
#[derive(Debug, Clone, Default)]
pub(crate) struct TicketEpisode {
    /// Rows tagged with the ticket, plus every row sharing a command id one
    /// of those rows names, deduplicated and sorted oldest first.
    pub(crate) rows: Vec<AuditRow>,
    /// Command ids of `open_order` completions reporting this ticket.
    pub(crate) open_commands: Vec<String>,
}

fn base_query(kinds: &[AuditKind], rows_cap: u32) -> Result<AuditQuery, AuditError> {
    AuditQuery::new(kinds, rows_cap).map_err(|error| AuditError::Storage {
        reason: format!("invalid ticket-episode query: {error}"),
    })
}

/// Command ids named by `payload.command_id`, valid UUIDs only, in
/// first-seen order without duplicates.
pub(crate) fn command_ids(rows: &[AuditRow]) -> Vec<String> {
    let mut ids = Vec::new();
    for row in rows {
        if let Some(id) = row.payload.get("command_id").and_then(Value::as_str)
            && CommandId::parse(id).is_some()
            && !ids.iter().any(|known: &String| known == id)
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

/// Whether a completed-command row reports `ticket` as its fill.
pub(crate) fn is_open_fill(row: &AuditRow, ticket: i64) -> bool {
    row.kind == "command_completed"
        && row.payload.get("kind").and_then(Value::as_str) == Some("open_order")
        && row
            .payload
            .get("result")
            .and_then(|result| result.get("ticket"))
            .and_then(Value::as_i64)
            == Some(ticket)
}

/// Sort key: parsed trail time, then the raw text for unparseable rows, then
/// row id, so replays and hand-built fixtures still order deterministically.
pub(crate) fn chronological(rows: &mut [AuditRow]) {
    rows.sort_by(|left, right| {
        parse_trail_time(&left.at)
            .cmp(&parse_trail_time(&right.at))
            .then_with(|| left.at.cmp(&right.at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Reads every row tagged with `ticket`, plus every row sharing a command id
/// one of those rows names (the entry decision only carries the open
/// command's id, not the ticket itself), deduplicated and sorted oldest
/// first.
///
/// # Errors
/// Returns [`AuditError`] when the trail cannot be read.
pub(crate) async fn ticket_episode(
    trail: &Arc<dyn AuditTrail>,
    kinds: &[AuditKind],
    rows_cap: u32,
    ticket: i64,
) -> Result<TicketEpisode, AuditError> {
    let ticket_query = base_query(kinds, rows_cap)?
        .with_ticket(ticket)
        .map_err(|error| AuditError::Storage {
            reason: error.to_string(),
        })?;
    let mut rows = trail.query(&ticket_query).await?;
    let open_commands = command_ids(
        &rows
            .iter()
            .filter(|row| is_open_fill(row, ticket))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let linked_ids = command_ids(&rows);
    if !linked_ids.is_empty() {
        let ids: Vec<String> = linked_ids
            .into_iter()
            .take(crate::audit::MAX_QUERY_COMMAND_IDS)
            .collect();
        let linked_query = base_query(kinds, rows_cap)?
            .with_command_ids(&ids)
            .map_err(|error| AuditError::Storage {
                reason: error.to_string(),
            })?;
        rows.extend(trail.query(&linked_query).await?);
    }
    let mut seen = BTreeSet::new();
    rows.retain(|row| seen.insert(row.id.clone()));
    chronological(&mut rows);
    Ok(TicketEpisode {
        rows,
        open_commands,
    })
}

/// The `proposal_evaluated` row that opened the ticket behind `episode`: its
/// `command_id` matches one of `episode`'s `open_commands`.
pub(crate) fn entry_decision<'a>(
    rows: &'a [AuditRow],
    open_commands: &[String],
) -> Option<&'a AuditRow> {
    rows.iter().find(|row| {
        row.kind == "proposal_evaluated"
            && row
                .payload
                .get("command_id")
                .and_then(Value::as_str)
                .is_some_and(|id| open_commands.iter().any(|open| open == id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEvent, MemoryTrail};
    use serde_json::json;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    fn instant(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("fixture time")
    }

    #[actix_web::test]
    async fn ticket_episode_links_the_entry_decision_by_its_open_command_id() {
        let trail = MemoryTrail::default();
        let open_id = uuid::Uuid::new_v4().to_string();
        trail.record_at(
            instant("2026-01-01T00:00:00Z"),
            AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "queued", "command_id": open_id, "rationale": "entry reason"}),
            ),
        );
        trail.record_at(
            instant("2026-01-01T00:00:05Z"),
            AuditEvent::new(
                AuditKind::CommandCompleted,
                json!({"kind": "open_order", "command_id": open_id, "result": {"ticket": 99}}),
            ),
        );
        trail.record_at(
            instant("2026-01-01T01:00:00Z"),
            AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "close_queued", "ticket": 99, "origin": "autopilot"}),
            ),
        );
        let dyn_trail: Arc<dyn AuditTrail> = Arc::new(trail);
        let episode = ticket_episode(&dyn_trail, &STORY_KINDS, STORY_ROWS, 99)
            .await
            .expect("episode");
        assert_eq!(episode.rows.len(), 3, "ticket rows plus the linked entry");
        assert_eq!(episode.open_commands, vec![open_id.clone()]);
        let entry = entry_decision(&episode.rows, &episode.open_commands).expect("entry");
        assert_eq!(entry.payload["rationale"], "entry reason");
        // Rows come back oldest first.
        assert_eq!(episode.rows[0].kind, "proposal_evaluated");
        assert_eq!(episode.rows[0].payload["outcome"], "queued");
        assert_eq!(
            episode.rows.last().expect("last").payload["outcome"],
            "close_queued"
        );
    }

    #[test]
    fn command_ids_are_valid_unique_and_ordered() {
        let row = |id: &str, kind: &str, command_id: &str| AuditRow {
            id: id.to_owned(),
            at: id.to_owned(),
            kind: kind.to_owned(),
            payload: json!({"command_id": command_id}),
        };
        let rows = vec![
            row(
                "a",
                "command_queued",
                "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10",
            ),
            row("b", "command_queued", "not-a-uuid"),
            row(
                "c",
                "command_completed",
                "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10",
            ),
        ];
        assert_eq!(
            command_ids(&rows),
            vec!["5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10".to_owned()]
        );
    }

    #[test]
    fn chronological_sorts_by_parsed_time_then_id() {
        let row = |id: &str, at: &str| AuditRow {
            id: id.to_owned(),
            at: at.to_owned(),
            kind: "x".to_owned(),
            payload: json!({}),
        };
        let mut rows = vec![
            row("b", "2026-09-24T06:00:00.000Z"),
            row("a", "2026-09-24 05:00:00+00"),
            row("c", "garbage"),
        ];
        chronological(&mut rows);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"],
            "unparseable times sort first, the rest chronologically"
        );
    }

    #[test]
    fn is_open_fill_matches_only_the_reported_ticket() {
        let row = AuditRow {
            id: "1".to_owned(),
            at: "t".to_owned(),
            kind: "command_completed".to_owned(),
            payload: json!({"kind": "open_order", "result": {"ticket": 5}}),
        };
        assert!(is_open_fill(&row, 5));
        assert!(!is_open_fill(&row, 6));
    }
}
