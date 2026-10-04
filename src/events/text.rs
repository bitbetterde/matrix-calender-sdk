//! Helpers for the MSC1767 `m.text` fallback block.
//!
//! The wording follows the MSC examples, for example
//! `Team standup – 2026-07-01 09:00–09:30 (Europe/Berlin)`.

use chrono::{Days, NaiveDate};
use matrix_sdk::ruma::events::message::TextContentBlock;
use serde::{Serialize, Serializer};

use super::{
    calendar_event::{CalendarEvent, Status},
    datetime::{DATE_FORMAT, EventTime},
    invite::{CalendarInvite, InviteEventSummary, InviteMethod},
    rsvp::PartStat,
};

/// Formats a `start`/`end` pair for humans, e.g. `2026-07-01 09:00–09:30 (Europe/Berlin)`.
///
/// All-day ranges are shown with an inclusive end date (the stored `end` is exclusive), e.g.
/// `2026-12-25 (all day)` or `2026-12-24 – 2026-12-26 (all day)`.
pub fn format_time_range(start: &EventTime, end: &EventTime) -> String {
    const HM: &str = "%H:%M";
    const DHM: &str = "%Y-%m-%d %H:%M";
    match (start, end) {
        (
            EventTime::Timed {
                localtime: s,
                timezone: stz,
                ..
            },
            EventTime::Timed {
                localtime: e,
                timezone: etz,
                ..
            },
        ) => {
            if stz != etz {
                format!(
                    "{} ({}) – {} ({})",
                    s.format(DHM),
                    stz.name(),
                    e.format(DHM),
                    etz.name()
                )
            } else if s.date() == e.date() {
                format!("{}–{} ({})", s.format(DHM), e.format(HM), stz.name())
            } else {
                format!("{} – {} ({})", s.format(DHM), e.format(DHM), stz.name())
            }
        }
        (EventTime::AllDay { date: s }, EventTime::AllDay { date: e }) => {
            let last = e.checked_sub_days(Days::new(1)).unwrap_or(*e);
            if last <= *s {
                format!("{} (all day)", fmt_date(s))
            } else {
                format!("{} – {} (all day)", fmt_date(s), fmt_date(&last))
            }
        }
        (s, e) => format!("{s} – {e}"),
    }
}

fn fmt_date(d: &NaiveDate) -> String {
    d.format(DATE_FORMAT).to_string()
}

/// Fallback text for a calendar event: `"{title} – {time range}"`, prefixed with
/// `Cancelled: ` for cancelled events.
pub fn calendar_event_text(event: &CalendarEvent) -> TextContentBlock {
    let prefix = if event.status == Status::Cancelled {
        "Cancelled: "
    } else {
        ""
    };
    TextContentBlock::plain(format!(
        "{prefix}{} – {}",
        event.title,
        format_time_range(&event.start, &event.end)
    ))
}

/// Fallback text for an invite, e.g.
/// `You are invited to: Team standup\n2026-07-01 09:00–09:30 (Europe/Berlin)\nOrganised by @alice:example.com`.
///
/// The `Organised by` line is omitted when the invite does not name an organiser.
pub fn invite_text(invite: &CalendarInvite) -> TextContentBlock {
    let ev = &invite.calendar_event;
    let lead = match &invite.method {
        InviteMethod::Request => "You are invited to",
        InviteMethod::Cancel => "Cancelled",
        InviteMethod::Counter => "Counter-proposal for",
        other => other.as_str(),
    };
    let mut body = format!(
        "{lead}: {}\n{}",
        ev.title,
        format_time_range(&ev.start, &ev.end)
    );
    if let Some(organizer) = &ev.organizer {
        body.push_str("\nOrganised by ");
        body.push_str(organizer.as_str());
    }
    TextContentBlock::plain(body)
}

/// Fallback text for an RSVP, e.g. `Accepted: Team standup on 2026-07-01`.
///
/// Without a summary of the invited event, a generic text such as
/// `Accepted the invitation` is produced.
pub fn rsvp_text(partstat: &PartStat, summary: Option<&InviteEventSummary>) -> TextContentBlock {
    let verb = match partstat {
        PartStat::Accepted => "Accepted",
        PartStat::Declined => "Declined",
        PartStat::Tentative => "Tentatively accepted",
        PartStat::NeedsAction => "Needs action",
        other => other.as_str(),
    };
    let body = match summary {
        Some(s) => format!("{verb}: {} on {}", s.title, fmt_date(&s.start.local_date())),
        None if *partstat == PartStat::NeedsAction => "Invitation needs action".to_owned(),
        None => format!("{verb} the invitation"),
    };
    TextContentBlock::plain(body)
}

/// Serializes a [`TextContentBlock`] with an explicit `mimetype` on every representation, as
/// in the MSC examples (`[{ "body": "...", "mimetype": "text/plain" }]`).
///
/// ruma's own serializer omits `"mimetype": "text/plain"` because it is the MSC1767 default;
/// both forms are equivalent on input.
pub(crate) fn serialize_text_block<S: Serializer>(
    block: &TextContentBlock,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Repr<'a> {
        body: &'a str,
        mimetype: &'a str,
    }

    serializer.collect_seq(block.iter().map(|r| Repr {
        body: &r.body,
        mimetype: &r.mimetype,
    }))
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::{Berlin, London};

    use super::*;

    fn timed(s: &str, tz: chrono_tz::Tz) -> EventTime {
        EventTime::timed(s.parse().unwrap(), tz).unwrap()
    }

    fn date(s: &str) -> EventTime {
        EventTime::all_day(s.parse().unwrap())
    }

    #[test]
    fn ranges() {
        assert_eq!(
            format_time_range(
                &timed("2026-07-01T09:00:00", Berlin),
                &timed("2026-07-01T09:30:00", Berlin)
            ),
            "2026-07-01 09:00–09:30 (Europe/Berlin)"
        );
        assert_eq!(
            format_time_range(
                &timed("2026-07-01T22:00:00", Berlin),
                &timed("2026-07-02T01:00:00", Berlin)
            ),
            "2026-07-01 22:00 – 2026-07-02 01:00 (Europe/Berlin)"
        );
        assert_eq!(
            format_time_range(
                &timed("2026-07-01T09:00:00", Berlin),
                &timed("2026-07-01T09:00:00", London)
            ),
            "2026-07-01 09:00 (Europe/Berlin) – 2026-07-01 09:00 (Europe/London)"
        );
        assert_eq!(
            format_time_range(&date("2026-12-25"), &date("2026-12-26")),
            "2026-12-25 (all day)"
        );
        assert_eq!(
            format_time_range(&date("2026-12-24"), &date("2026-12-27")),
            "2026-12-24 – 2026-12-26 (all day)"
        );
    }
}
