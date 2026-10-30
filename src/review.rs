//! Turn the single model response into a verified, compatible `domain.Report`.
//!
//! The model is untrusted output. Even though it ran on untrusted input behind
//! a hardened prompt, we re-verify its claims here rather than trust them:
//!   * Every finding MUST cite a location that lies on a changed target line
//!     (`ChangeSet.Target` semantics). Findings off the change are discarded
//!     and counted in a presentation note.
//!   * Finding IDs are engine-derived (deterministic `F-<hash>`), so re-reviews
//!     dedup on GitHub; the model never supplies ids.
//!   * The diagram is validated for size/shape; labels are left intact in the
//!     structured data and only sanitized at Mermaid render time.
//!   * Severity must be P0..P3 (guaranteed by the typed decode).

use sha2::{Digest, Sha256};

use crate::domain::{
    Decision, Diagram, DiagramEdge, DiagramNode, Finding, Location, Report, ReviewExclusions,
    ReviewRun, Severity, SourceRevision,
};
use crate::git::DiffSummary;
use crate::model::{RawDiagram, RawReview};

/// Does `[start,end]` on the target side intersect a changed line? Mirrors Go
/// `ChangeSet.Target`.
fn on_changed_line(diff: &DiffSummary, path: &str, start: u32, end: u32) -> bool {
    for f in &diff.files {
        if f.path != path {
            continue;
        }
        if f.unknown {
            return true;
        }
        for e in &f.edits {
            if e.target_lines > 0
                && start <= e.target_start + e.target_lines - 1
                && end >= e.target_start
            {
                return true;
            }
            // Deletion point: a zero-length target range borders target_start.
            if e.target_lines == 0 && start <= e.target_start && end >= e.target_start.saturating_sub(1)
            {
                return true;
            }
        }
    }
    false
}

/// Build the final report from the single model response.
pub fn build_report(
    run_id: &str,
    target: &SourceRevision,
    base: &SourceRevision,
    model_provider: &str,
    model_id: &str,
    diff: &DiffSummary,
    raw: RawReview,
) -> Report {
    let mut findings: Vec<Finding> = Vec::new();
    let mut discarded = 0usize;
    let mut notes: Vec<String> = Vec::new();

    for rf in raw.findings {
        let loc = Location {
            path: rf.path.clone(),
            start_line: rf.start_line,
            end_line: rf.end_line,
        };
        if !loc.valid() || !on_changed_line(diff, &loc.path, loc.start_line, loc.end_line) {
            discarded += 1;
            continue;
        }
        let id = finding_id(target, &loc, &rf.title, &rf.reason);
        findings.push(Finding {
            advisory: None,
            id,
            title: rf.title,
            severity: rf.severity,
            reason: rf.reason,
            suggestion: rf.suggestion,
            verification: rf.verification,
            revision: target.clone(),
            location: loc,
            evidence_ids: Vec::new(),
        });
    }

    // Deterministic order: P0 first, then by id.
    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.id.cmp(&b.id)));

    if discarded > 0 {
        notes.push(format!(
            "Discarded {discarded} candidate finding(s) that did not cite a changed line."
        ));
    }

    // Decision: engine-derived, not taken from the model, so a prompt-injected
    // "ready_for_human" cannot override real findings.
    let has_blocking = findings
        .iter()
        .any(|f| f.severity != Severity::Low);
    let decision = if has_blocking {
        Decision::NeedsChanges
    } else {
        Decision::ReadyForReview
    };

    // Scores, clamped to 1..=5, prose sanitized (dash-free, bounded).
    let map_score = |s: crate::model::RawScore| crate::domain::Score {
        score: s.score.clamp(1, 5),
        rationale: sanitize_text(&s.rationale, 400),
        to_improve: sanitize_text(&s.to_improve, 500),
    };
    let scores = raw.scores.map(|s| crate::domain::Scores {
        code_quality: map_score(s.code_quality),
        security: map_score(s.security),
    });

    let diagram = raw.diagram.and_then(|d| validate_diagram(d, &mut notes));

    // Carry alignment through, bounding and sanitizing the model's prose. The
    // level is normalized to the allowed set; anything else becomes "unclear".
    let change_alignment = raw.change_alignment.map(|a| {
        let level = match a.level.as_str() {
            "aligned" | "partial" | "diverging" => a.level,
            _ => "unclear".to_string(),
        };
        crate::domain::ChangeAlignment {
            level,
            rationale: sanitize_text(&a.rationale, 600),
        }
    });

    Report {
        presentation: None,
        schema_version: "1".into(),
        run: ReviewRun {
            id: run_id.to_string(),
            status: "completed".into(),
            freshness: "current".into(),
            coverage_complete: true,
            target: target.clone(),
            base: base.clone(),
            model: crate::domain::ModelSelection {
                provider: model_provider.into(),
                model_id: model_id.into(),
            },
            exclusions: None,
            review_notes: Vec::new(),
        },
        summary: sanitize_text(&raw.summary, 4000),
        decision,
        change_alignment,
        scores,
        findings,
        verification_gaps: Vec::new(),
        diagram,
        presentation_notes: notes,
    }
}

/// Attach the `.inkspectignore` scope to a report: the structured metadata
/// whenever a policy exists, and a short note when it excluded files.
pub fn with_exclusions(mut report: Report, exclusions: Option<ReviewExclusions>) -> Report {
    if let Some(e) = exclusions.as_ref().filter(|e| e.excluded_files > 0) {
        report.presentation_notes.push(e.note());
    }
    report.run.exclusions = exclusions;
    report
}

/// The report when `.inkspectignore` excludes every changed file. No model is
/// called, so there is nothing to verify and nothing to charge.
pub fn excluded_report(
    run_id: &str,
    target: &SourceRevision,
    base: &SourceRevision,
    model_provider: &str,
    model_id: &str,
    exclusions: ReviewExclusions,
) -> Report {
    let report = Report {
        presentation: None,
        schema_version: "1".into(),
        run: ReviewRun {
            id: run_id.to_string(),
            status: "completed".into(),
            freshness: "current".into(),
            coverage_complete: true,
            target: target.clone(),
            base: base.clone(),
            model: crate::domain::ModelSelection {
                provider: model_provider.into(),
                model_id: model_id.into(),
            },
            exclusions: None,
            review_notes: Vec::new(),
        },
        summary: "Every changed file is excluded by the repository's .inkspectignore, so there was nothing to review.".into(),
        decision: Decision::ReadyForReview,
        change_alignment: None,
        scores: None,
        findings: Vec::new(),
        verification_gaps: Vec::new(),
        diagram: None,
        presentation_notes: Vec::new(),
    };
    with_exclusions(report, Some(exclusions))
}

/// Validate the model diagram into the domain shape, or drop it with a note.
fn validate_diagram(d: RawDiagram, notes: &mut Vec<String>) -> Option<Diagram> {
    if !d.meaningful {
        return None;
    }
    let title = d.title.unwrap_or_default();
    let view = d.view.unwrap_or_else(|| "current".into());
    if title.trim().is_empty()
        || title.len() > 200
        || (view != "current" && view != "proposed")
        || d.nodes.len() < 2
        || d.nodes.len() > 30
        || d.edges.is_empty()
        || d.edges.len() > 60
    {
        notes.push("The behavior diagram was omitted: it did not meet the size/shape rules.".into());
        return None;
    }
    let mut ids = std::collections::HashSet::new();
    let mut nodes = Vec::with_capacity(d.nodes.len());
    for n in d.nodes {
        if n.id.is_empty()
            || n.id.len() > 64
            || n.label.trim().is_empty()
            || n.label.len() > 200
            || !ids.insert(n.id.clone())
        {
            notes.push("The behavior diagram was omitted: invalid node.".into());
            return None;
        }
        nodes.push(DiagramNode {
            evidence_ids: Vec::new(),
            id: n.id,
            label: n.label,
            issue: n.issue,
        });
    }
    let mut edges = Vec::with_capacity(d.edges.len());
    for e in d.edges {
        if !ids.contains(&e.from) || !ids.contains(&e.to) || e.label.len() > 200 {
            notes.push("The behavior diagram was omitted: invalid edge.".into());
            return None;
        }
        edges.push(DiagramEdge {
            evidence_ids: Vec::new(),
            from: e.from,
            to: e.to,
            label: e.label,
            missing: e.missing,
        });
    }
    Some(Diagram {
        title,
        view,
        nodes,
        edges,
        evidence_ids: Vec::new(),
    })
}

/// Deterministic finding id: `F-` + first 16 hex of sha256 over the identifying
/// fields, so identical findings across re-runs collide and dedup.
fn finding_id(rev: &SourceRevision, loc: &Location, title: &str, reason: &str) -> String {
    let mut h = Sha256::new();
    h.update(rev.revision_id.as_bytes());
    h.update(b"\0");
    h.update(loc.path.as_bytes());
    h.update(format!("\0{}-{}\0", loc.start_line, loc.end_line).as_bytes());
    h.update(normalize_ws(title).as_bytes());
    h.update(b"\0");
    h.update(normalize_ws(reason).as_bytes());
    let hex = hex::encode(h.finalize());
    format!("F-{}", &hex[..16])
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse control characters, replace dashes used as separators, and bound
/// the length of model-authored prose. Per product style, no dash characters
/// appear in output: em dash and en dash become a semicolon; a spaced hyphen
/// used as a separator ( - ) becomes a comma. In-word hyphens (e.g. "human-player")
/// are preserved.
pub fn sanitize_text(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        match c {
            _ if c.is_control() && c != '\n' => {
                out.push(' ');
            }
            // Em dash / en dash / horizontal bar / minus sign: treat as a clause
            // separator.
            '\u{2014}' | '\u{2013}' | '\u{2015}' | '\u{2212}' => {
                // Drop a preceding space, then write "; ".
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push_str("; ");
                // Skip a following space.
                while chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            // ASCII hyphen used as a separator: " - " -> ", ".
            '-' if prev == Some(' ') && chars.peek() == Some(&' ') => {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push_str(", ");
                chars.next(); // consume the trailing space
            }
            _ => out.push(c),
        }
        prev = Some(c);
    }
    crate::retrieval::bounded(&out, max)
}

/// Render the structured diagram into Mermaid, generating node ids (`n{i}`) and
/// sanitizing labels so model/source text cannot inject Mermaid syntax. Mirrors
/// the Go engine's `report.Mermaid` + `mermaidLabel`.
pub fn mermaid(d: &Diagram) -> String {
    let mut b = String::from("flowchart LR\n");
    let mut index = std::collections::HashMap::new();
    for (i, n) in d.nodes.iter().enumerate() {
        index.insert(n.id.clone(), i);
        b.push_str(&format!("    n{i}[\"{}\"]\n", mermaid_label(&n.label)));
    }
    for e in &d.edges {
        let (Some(&from), Some(&to)) = (index.get(&e.from), index.get(&e.to)) else {
            continue;
        };
        let arrow = if e.missing { "-.->" } else { "-->" };
        if e.label.is_empty() {
            b.push_str(&format!("    n{from} {arrow} n{to}\n"));
        } else {
            b.push_str(&format!(
                "    n{from} {arrow}|\"{}\"| n{to}\n",
                mermaid_label(&e.label)
            ));
        }
    }
    b
}

/// Replace Mermaid/HTML syntax delimiters with display-equivalent glyphs so a
/// malicious label cannot break out of its quoted node/edge. Matches the Go
/// `mermaidLabel` substitution set.
fn mermaid_label(text: &str) -> String {
    let one_line: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let runes: Vec<char> = one_line.chars().collect();
    let mut out = String::new();
    for (i, &r) in runes.iter().enumerate() {
        if r == '&' {
            let prev = i.checked_sub(1).map(|j| runes[j]) == Some('&');
            let next = runes.get(i + 1) == Some(&'&');
            if prev || next {
                out.push('&');
            } else {
                out.push('＆');
            }
        } else if r.is_alphanumeric()
            || " -.:?_=+/,()'!|".contains(r)
        {
            out.push(r);
        } else {
            out.push(match r {
                '"' => '”',
                '<' => '＜',
                '>' => '＞',
                '`' => '’',
                '\\' => '∖',
                '#' => '＃',
                '%' => '％',
                '[' => '［',
                ']' => '］',
                '{' => '｛',
                '}' => '｝',
                ';' => '；',
                other => other,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev() -> SourceRevision {
        SourceRevision {
            tenant_id: "t".into(),
            provider: "github".into(),
            provider_instance: "github.com".into(),
            repository_id: "1".into(),
            revision_id: "abc".into(),
        }
    }

    #[test]
    fn mermaid_label_neutralizes_injection() {
        let label = r#"x"] click x "javascript:alert(1)"#;
        let rendered = mermaid_label(label);
        assert!(!rendered.contains("\"]"));
        assert!(rendered.contains('”'));
    }

    #[test]
    fn mermaid_keeps_boolean_operators() {
        assert!(mermaid_label("a && b").contains("&&"));
    }

    #[test]
    fn finding_id_is_deterministic() {
        let loc = Location { path: "a.rs".into(), start_line: 1, end_line: 2 };
        let a = finding_id(&rev(), &loc, "t", "r");
        let b = finding_id(&rev(), &loc, "t", "r");
        assert_eq!(a, b);
        assert!(a.starts_with("F-"));
    }
}
