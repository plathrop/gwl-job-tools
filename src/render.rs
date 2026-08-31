//! Human-readable output rendering: the lead card (termimad markdown), the
//! ranked queue (crossterm), and the review prompt.

use std::io::Write;

use crossterm::style::Color;
use miette::{IntoDiagnostic, Result};
use termimad::MadSkin;
use tracing::instrument;

use crate::{domain::events::CheatSheetEntry, projections::LeadRecord};

/// Strip control characters (C0/C1, including ANSI/OSC escape sequences) from
/// record-derived text before it reaches the terminal. Posting text is
/// untrusted (remote job boards), so it must not be able to inject terminal
/// control sequences.
pub(crate) fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Linearly interpolate a score (0–100) through red → yellow → green.
pub fn score_rgb(score: u64) -> (u8, u8, u8) {
    debug_assert!(score <= 100, "score {score} exceeds the 0–100 contract");
    let s = score.clamp(0, 100) as f32;
    let (r, g) = if s <= 50.0 {
        (255.0, s * 255.0 / 50.0)
    } else {
        (255.0 - (s - 50.0) * 255.0 / 50.0, 255.0)
    };
    (r as u8, g as u8, 0)
}

/// The crossterm color for a score.
pub fn score_color(score: u64) -> Color {
    let (r, g, b) = score_rgb(score);
    Color::Rgb { r, g, b }
}

/// The markdown for a lead's card (design doc 0001 §5). The score line is
/// the only bold text, so the caller can color it via the skin's `bold`
/// style.
pub fn card_markdown(record: &LeadRecord) -> String {
    let mut md = String::new();

    // Header: title - company - lead prefix (8 chars, decision 0008). The
    // company is omitted when the title already ends with it (some boards
    // keep the full "Title — Company" string in the title).
    let title = sanitize(record.extracted.title.as_deref().unwrap_or("Untitled"));
    let lead_prefix: String = record.lead_id.to_string().chars().take(8).collect();
    match record.extracted.company.as_deref() {
        Some(company) => {
            let company = sanitize(company);
            if title.to_lowercase().ends_with(&company.to_lowercase()) {
                md.push_str(&format!("# {title} - {lead_prefix}\n\n"));
            } else {
                md.push_str(&format!("# {title} - {company} - {lead_prefix}\n\n"));
            }
        }
        None => md.push_str(&format!("# {title} - {lead_prefix}\n\n")),
    }

    // Score or rejection.
    if let Some(score) = &record.latest_score {
        md.push_str(&format!("**Score: {}**\n", score.composite));
        md.push_str(&format!("`{}`\n\n", sanitize(&score.breakdown)));
    } else if let Some(rejection) = &record.latest_rejection {
        md.push_str(&format!("**Rejected: {}**\n", sanitize(&rejection.gate)));
        md.push_str(&format!("`{}`\n\n", sanitize(&rejection.reason)));
    }

    // Table (conditional rows).
    md.push_str("| Field | Value |\n|---|---|\n");
    if let Some(location) = record.extracted.location.as_deref() {
        md.push_str(&format!("| Location | {} |\n", sanitize(location)));
    }
    let remote = match record.extracted.remote {
        Some(true) => "Yes",
        Some(false) => "No",
        None => "Unknown",
    };
    md.push_str(&format!("| Remote | {remote} |\n"));
    if let Some(comp) = &record.extracted.comp {
        md.push_str(&format!("| Compensation | {} |\n", sanitize(&comp.raw)));
    }
    if record.deferral_count > 0 {
        md.push_str(&format!("| Deferral Count | {} |\n", record.deferral_count));
    }
    // The derived lifecycle status (design doc 0002): one dimension — not
    // the mark (a decision) and the outcome (a fact) side by side. The
    // underlying facts stay visible via `show --json` and `events`.
    md.push_str(&format!(
        "| Status | {} |\n",
        sanitize(&record.lifecycle_status())
    ));
    if let Some(source) = record.source.as_deref() {
        md.push_str(&format!("| Source | {} |\n", sanitize(source)));
    }
    md.push('\n');

    // URL.
    if let Some(url) = record.url.as_deref() {
        md.push_str(&sanitize(url));
        md.push('\n');
    }

    md
}

/// Render a lead's card to stdout.
#[instrument(skip_all, fields(lead_id = %record.lead_id))]
pub fn render_card(record: &LeadRecord, color: bool) -> Result<()> {
    let md = card_markdown(record);
    let mut skin = if color {
        MadSkin::default()
    } else {
        MadSkin::no_style()
    };
    if color && let Some(score) = &record.latest_score {
        skin.bold.set_fg(score_color(score.composite));
    }
    skin.write_text(&md).into_diagnostic()?;
    Ok(())
}

/// Render the ranked queue to stdout (rank, colored score, title @ company,
/// deferral count, derived status, lead prefix).
pub fn render_list(records: &[&LeadRecord], color: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for (i, record) in records.iter().enumerate() {
        writeln!(out, "{}", list_line(i + 1, record, color)).into_diagnostic()?;
    }
    Ok(())
}

/// One line of the ranked queue.
fn list_line(rank: usize, record: &LeadRecord, color: bool) -> String {
    let title = sanitize(record.extracted.title.as_deref().unwrap_or("Untitled"));
    let company = sanitize(record.extracted.company.as_deref().unwrap_or(""));

    let score = match &record.latest_score {
        Some(score) => colored_score(score.composite, color),
        None => "  -".to_string(),
    };

    let mut line = format!("{rank:>3}  {score}  {title}");
    if !company.is_empty() {
        line.push_str(&format!(" @ {company}"));
    }
    if record.deferral_count > 0 {
        line.push_str(&format!("  (deferred {}×)", record.deferral_count));
    }
    // One derived status tag (design doc 0002) — not the mark and the
    // outcome as coequal parallel tags.
    line.push_str(&format!("  [{}]", sanitize(&record.lifecycle_status())));
    // The 8-char lead prefix is the addressing handle for `mark`/`show`.
    let lead_prefix: String = record.lead_id.to_string().chars().take(8).collect();
    line.push_str(&format!("  [{lead_prefix}]"));
    line
}

/// A score rendered with a 24-bit RGB foreground when color is on.
fn colored_score(score: u64, color: bool) -> String {
    if color {
        let (r, g, b) = score_rgb(score);
        format!("\x1b[38;2;{r};{g};{b}m{score:>3}\x1b[0m")
    } else {
        format!("{score:>3}")
    }
}

/// The review prompt line: single-key actions with the key accented (color)
/// or parenthesized (no color).
pub fn render_prompt(color: bool) -> String {
    const ACTIONS: [(&str, &str); 6] = [
        ("a", "uto"),
        ("m", "anual"),
        ("d", "efer"),
        ("i", "gnore"),
        ("s", "kip"),
        ("q", "uit"),
    ];
    let parts: Vec<String> = ACTIONS
        .iter()
        .map(|(key, rest)| {
            if color {
                format!("\x1b[1;36m{key}\x1b[0m{rest}")
            } else {
                format!("({key}){rest}")
            }
        })
        .collect();
    parts.join(" | ")
}

/// The ATS cheat sheet, shown after an `apply-automatically` mark so the
/// answers are visible while completing the opened form.
pub fn render_cheat_sheet(entries: &[CheatSheetEntry]) -> String {
    let mut s = String::from("Cheat sheet:\n");
    for entry in entries {
        s.push_str(&format!("  {}: {}\n", entry.question, entry.answer));
    }
    s
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use uuid::Uuid;

    use super::*;
    use crate::{
        domain::events::{CompRange, ExtractedFields, Identifiers, ScoredPayload},
        projections::LeadRecord,
    };

    fn lead_record() -> LeadRecord {
        LeadRecord {
            lead_id: Uuid::now_v7(),
            dedupe_key: None,
            identifiers: Identifiers::default(),
            adapter: None,
            source: Some("search".into()),
            url: Some("https://example.com/j".into()),
            extracted: ExtractedFields {
                title: Some("Staff Engineer".into()),
                company: Some("Acme".into()),
                location: Some("Remote, US".into()),
                remote: Some(true),
                comp: Some(CompRange {
                    min: Some(200_000),
                    max: Some(250_000),
                    currency: "USD".into(),
                    period: "year".into(),
                    raw: "$200,000 - $250,000".into(),
                }),
                ..Default::default()
            },
            latest_mark: None,
            deferral_count: 0,
            apply_queued: false,
            latest_rejection: None,
            latest_score: Some(ScoredPayload {
                composite: 75,
                revision: 1,
                dimensions: vec![],
                breakdown: "75 = 0.5·level(80) + 0.5·remote(70)".into(),
            }),
            latest_outcome: None,
            event_count: 0,
            first_seen: Timestamp::now(),
            last_event: Timestamp::now(),
        }
    }

    #[test]
    fn score_rgb_gradient() {
        assert_eq!(score_rgb(0), (255, 0, 0)); // red
        assert_eq!(score_rgb(50), (255, 255, 0)); // yellow
        assert_eq!(score_rgb(100), (0, 255, 0)); // green
        assert_eq!(score_rgb(25), (255, 127, 0)); // orange-ish
    }

    #[test]
    fn card_markdown_has_header_score_and_table() {
        let md = card_markdown(&lead_record());
        assert!(md.contains("# Staff Engineer - Acme - "));
        assert!(md.contains("**Score: 75**"));
        assert!(md.contains("| Location | Remote, US |"));
        assert!(md.contains("| Remote | Yes |"));
        assert!(md.contains("| Source | search |"));
        assert!(md.contains("https://example.com/j"));
    }

    #[test]
    fn card_shows_derived_status_not_mark_and_outcome_rows() {
        // Design doc 0002: the card renders ONE lifecycle dimension. A lead
        // with both a decision mark and a recorded outcome shows a single
        // derived status row — not Mark and Outcome side by side.
        let mut record = lead_record();
        record.latest_mark = Some("apply-manual".into());
        record.latest_outcome = Some(crate::projections::OutcomeView {
            event_type: "applied".into(),
            note: None,
            method: None,
            occurred_at: Timestamp::now(),
        });
        let md = card_markdown(&record);
        assert!(md.contains("| Status | applied (manual) |"), "md: {md}");
        assert!(!md.contains("| Mark |"));
        assert!(!md.contains("| Outcome |"));
    }

    #[test]
    fn list_line_shows_one_status_tag() {
        // Not the mark and the outcome as coequal parallel tags: a lead
        // marked apply-manual and then recorded applied reads as one
        // derived tag.
        let mut record = lead_record();
        record.latest_mark = Some("apply-manual".into());
        record.latest_outcome = Some(crate::projections::OutcomeView {
            event_type: "applied".into(),
            note: None,
            method: Some("manual".into()),
            occurred_at: Timestamp::now(),
        });
        let line = list_line(1, &record, false);
        assert!(line.contains("[applied (manual)]"), "line: {line}");
        assert!(!line.contains("[apply-manual]"));
        // Unmarked scored lead: the pending stage.
        let line = list_line(1, &lead_record(), false);
        assert!(line.contains("[pending]"), "line: {line}");
    }

    #[test]
    fn sanitize_strips_control_characters() {
        // ESC is stripped (making the sequence inert); the printable
        // remainder stays. Newlines/tabs are stripped too (single-line
        // fields).
        assert_eq!(
            sanitize("hello\u{1b}[31mworld\u{1b}[0m"),
            "hello[31mworld[0m"
        );
        assert_eq!(sanitize("a\nb\tc"), "abc");
        assert_eq!(sanitize("plain"), "plain");
    }

    #[test]
    fn card_header_omits_company_when_title_ends_with_it() {
        // Some boards keep the full "Title — Company" string in the title;
        // the header must not append the company again.
        let mut record = lead_record();
        record.extracted.title = Some("Staff SRE — Alpha Co".into());
        record.extracted.company = Some("Alpha Co".into());
        let md = card_markdown(&record);
        assert!(md.contains("# Staff SRE — Alpha Co - "));
        assert!(!md.contains("Alpha Co - Alpha Co"));
    }

    #[test]
    fn list_line_includes_lead_prefix() {
        // The 8-char prefix is the addressing handle for `mark`/`show`.
        let record = lead_record();
        let prefix: String = record.lead_id.to_string().chars().take(8).collect();
        let line = list_line(1, &record, false);
        assert!(line.contains(&format!("[{prefix}]")), "line: {line}");
    }

    #[test]
    fn render_prompt_colors_keys() {
        let colored = render_prompt(true);
        assert!(colored.contains("\u{1b}[1;36ma\u{1b}[0muto"));
        let plain = render_prompt(false);
        assert!(plain.contains("(a)uto"));
        assert!(!plain.contains('\u{1b}'));
    }
}
