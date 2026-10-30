//! Render a `Report` to Markdown for GitHub (check summary / PR comment body)
//! and the CLI. Mermaid is produced from the structured diagram via
//! `crate::review::mermaid`, which generates safe node ids and sanitizes labels.
//!
//! All model/repository-authored text is escaped for Markdown and GitHub
//! mentions before embedding. Code fences grow to exceed any backtick run in
//! the content so a crafted snippet cannot close its enclosing block.

use crate::domain::{Decision, Finding, Presentation, Report, Severity};
use crate::review::{mermaid, sanitize_text};

/// Engine version shown in the footer. Tracks the crate version.
pub const ENGINE_VERSION: &str = match option_env!("INKSPECT_ENGINE_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

/// One source for CLI output, persisted reports, and both GitHub publishers.
pub fn attach(report: &mut Report) {
    let full = markdown(report);
    let mut summary = full.clone();
    if summary.len() > 58_000 {
        let mut shortened = report.clone();
        shortened.presentation = None;
        while summary.len() > 58_000 && !shortened.findings.is_empty() {
            shortened.findings.pop();
            summary = render_markdown(&shortened, true);
        }
    }
    let finding_prompts: std::collections::BTreeMap<_, _> = report.findings.iter().map(|f| (
        f.id.clone(), format!("{}\n{}:{}\n{}\n{}\n{}", f.title, f.location.path, f.location.start_line, f.reason, f.suggestion, f.verification)
    )).collect();
    let mut fix_parts: Vec<_> = report.findings.iter().filter_map(|f| finding_prompts.get(&f.id).cloned()).collect();
    fix_parts.extend(report.verification_gaps.iter().map(|g| format!("Verification gap: {}", g.reason)));
    report.presentation = Some(Presentation {
        version: 1,
        markdown: full,
        github_summary: summary,
        github_findings: report.findings.iter().map(|f| (f.id.clone(), finding_comment(f))).collect(),
        finding_prompts,
        fix_prompt: fix_parts.join("\n\n"),
    });
}

pub fn finding_comment(f: &Finding) -> String {
    format!("**{} · {}**\n\n{}\n\n**Suggested fix**\n{}\n\n**Verification**\n{}\n\nReply “fixed” after pushing your changes to request another review.",
        severity_label(f.severity), md(&sanitize_text(&f.title, 500)), md(&sanitize_text(&f.reason, 3000)),
        md(&sanitize_text(&f.suggestion, 3000)), md(&sanitize_text(&f.verification, 3000)))
}

fn report_link(report: &Report) -> String {
    let origin = std::env::var("INKSPECT_APP_ORIGIN").unwrap_or_else(|_| "https://app.inkspect.si".into());
    let mut url = reqwest::Url::parse(&origin).ok().filter(|u| matches!(u.scheme(), "https" | "http") && u.host_str().is_some() && u.username().is_empty() && u.password().is_none())
        .unwrap_or_else(|| reqwest::Url::parse("https://app.inkspect.si").unwrap());
    url.set_path("/dashboard"); url.set_query(None); url.set_fragment(None);
    url.query_pairs_mut().append_pair("view", "reviews").append_pair("repositoryId", &report.run.target.repository_id).append_pair("jobId", &report.run.id);
    // URL encoding alone leaves parentheses meaningful in Markdown links.
    url.to_string().replace('(', "%28").replace(')', "%29")
}

/// Render the report as a Markdown document for the PR comment / CLI.
pub fn markdown(report: &Report) -> String {
    render_markdown(report, false)
}

fn render_markdown(report: &Report, omitted_findings: bool) -> String {
    let mut b = String::new();

    b.push_str(&format!("## {}\n\n", headline(report.decision)));
    b.push_str(&format!("{}: {}\n\n", if report.run.target.provider == "local" {"Snapshot"} else {"Commit"}, md(&report.run.target.revision_id)));
    b.push_str(&md(&report.summary));
    b.push_str("\n\n");

    if !report.run.review_notes.is_empty() {
        b.push_str("**Review requests in this run**\n\n");
        for note in &report.run.review_notes {
            b.push_str(&format!("- {}: {}\n", md(&note.author), md(&note.body)));
        }
        b.push('\n');
    }

    if let Some(a) = &report.change_alignment {
        b.push_str(&format!(
            "**Change vs. PR intent:** {}. {}\n\n",
            alignment_label(&a.level),
            md(&a.rationale)
        ));
    }

    // Quality scores as a stacked list (no table): a table wraps badly in
    // narrow panels. Each area shows a visual bar, the n/5 value, and the
    // reason. For any score below 5/5, a collapsible "AI prompt" block follows,
    // combining the reason and the improvement so a user can hand it to an agent.
    if let Some(s) = &report.scores {
        b.push_str("### Scores\n\n");
        b.push_str(&score_block("Code quality", &s.code_quality));
        b.push_str(&score_block("Security", &s.security));
        b.push_str(&improvement_prompt("Code quality", &s.code_quality));
        b.push_str(&improvement_prompt("Security", &s.security));
    }

    if !report.run.coverage_complete || report.run.status != "completed" {
        b.push_str("> Coverage is incomplete; some areas could not be fully reviewed.\n\n");
    }

    // Behavior diagram (if present), shown directly under its own title as a
    // Mermaid block GitHub renders natively.
    if let Some(diagram) = &report.diagram {
        b.push_str(&format!(
            "### {}\n\n{}\n\n",
            md(&diagram.title),
            fenced("mermaid", &mermaid(diagram)),
        ));
    }

    if report.findings.is_empty() && omitted_findings {
        // A shortened comment must never claim that the review had no findings.
    } else if report.findings.is_empty() {
        b.push_str("No findings on the changed lines.\n\n");
    } else {
        b.push_str("### Findings\n\n");
        for f in &report.findings {
            b.push_str(&format!(
                "#### {} · {}\n\n",
                severity_label(f.severity),
                md(&f.title)
            ));
            b.push_str(&format!(
                "Location: `{}:{}..{}`\n\n",
                md(&f.location.path),
                f.location.start_line,
                f.location.end_line
            ));
            b.push_str(&format!("**Reason:** {}\n\n", md(&f.reason)));
            b.push_str(&format!("**Suggested fix:** {}\n\n", md(&f.suggestion)));
            b.push_str(&format!("**Verification:** {}\n\n", md(&f.verification)));
        }
    }

    for gap in &report.verification_gaps {
        b.push_str(&format!("> Verification gap: {}\n\n", md(&gap.reason)));
    }

    // Trusted scope metadata (a count and a commit id), not model text.
    if let Some(e) = report.run.exclusions.as_ref().filter(|e| e.excluded_files > 0) {
        b.push_str(&format!("> {}\n\n", md(&e.note())));
    }

    for note in &report.presentation_notes {
        b.push_str(&format!("> {}\n\n", md(note)));
    }
    if omitted_findings {
        b.push_str("> This comment omits findings to fit GitHub's limit. Open the full report in Inkspect for all findings.\n\n");
    }

    if report.run.target.provider != "local" {
        b.push_str(&format!("[View the full report in Inkspect]({})\n\n", report_link(report)));
    }

    // Footer: our own attribution and site link. No model details, no dashes.
    b.push_str(&format!(
        "---\n\nReviewed by [Inkspect](https://inkspect.si) · engine v{}\n",
        ENGINE_VERSION,
    ));

    b
}

/// A 5-point score as filled/empty blocks plus the numeric value.
fn score_bar(score: u8) -> String {
    let n = score.clamp(0, 5) as usize;
    format!("{}{} {n}/5", "█".repeat(n), "░".repeat(5 - n))
}

/// A stacked score entry (no table). One bold line with the area, bar and
/// value, then the reason on the next line. Renders cleanly at any width.
fn score_block(area: &str, s: &crate::domain::Score) -> String {
    let mut out = format!("**{}** {}\n", md(area), score_bar(s.score));
    if !s.rationale.trim().is_empty() {
        out.push_str(&format!("{}\n", md(&s.rationale)));
    }
    out.push('\n');
    out
}

/// For a sub-5 score, a collapsible block holding a ready-to-use prompt that
/// combines the "why" and the concrete improvement, so a user can paste it into
/// their own coding agent with full context. Nothing is emitted for 5/5.
fn improvement_prompt(area: &str, s: &crate::domain::Score) -> String {
    if s.score >= 5 || s.to_improve.trim().is_empty() {
        return String::new();
    }
    // The prompt text itself is sanitized (dash-free) and escaped. It is placed
    // in a fenced block so it is easy to copy.
    let mut prompt = String::new();
    prompt.push_str(&format!(
        "Improve the {} of this change to a 5/5 standard.\n\n",
        area.to_lowercase()
    ));
    if !s.rationale.trim().is_empty() {
        prompt.push_str(&format!("Context: {}\n\n", sanitize_text(&s.rationale, 400)));
    }
    prompt.push_str(&format!("Task: {}", sanitize_text(&s.to_improve, 500)));

    format!(
        "<details>\n<summary>AI prompt to reach 5/5 on {}</summary>\n\n{}\n</details>\n\n",
        md(area),
        fenced("text", &prompt),
    )
}

/// The headline reflects the review outcome, not who reviews next.
fn headline(d: Decision) -> &'static str {
    match d {
        Decision::NeedsChanges => "Changes requested",
        Decision::NeedsEvidence => "Needs verification",
        Decision::ReadyForReview => "No blocking issues found",
    }
}

fn alignment_label(level: &str) -> &'static str {
    match level {
        "aligned" => "Aligned",
        "partial" => "Partially aligned",
        "diverging" => "Diverging from stated intent",
        _ => "Unclear",
    }
}

fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "P0",
        Severity::High => "P1",
        Severity::Medium => "P2",
        Severity::Low => "P3",
    }
}

/// Escape Markdown control characters, strip separator dashes (product style:
/// no dash characters in output), and collapse to a single line.
fn md(text: &str) -> String {
    // sanitize_text replaces em/en dashes and spaced hyphens with ; or ,.
    let dashless = sanitize_text(text, usize::MAX);
    let one_line: String = dashless.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(one_line.len());
    for c in one_line.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '#' | '!' => {
                out.push('\\');
                out.push(c);
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '@' => out.push_str("@\u{200b}"),
            _ => out.push(c),
        }
    }
    out
}

/// Build a fenced code block whose fence is longer than any backtick run in the
/// content, so content can never terminate the block early.
fn fenced(language: &str, text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{language}\n{}\n{fence}", text.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;

    #[test]
    fn fence_grows_past_inner_backticks() {
        let out = fenced("mermaid", "a ``` b");
        assert!(out.starts_with("````mermaid"));
    }

    fn sample_report() -> Report {
        Report {
            presentation: None,
            schema_version: "1".into(),
            run: ReviewRun {
                id: "J-1".into(),
                status: "completed".into(),
                freshness: "current".into(),
                coverage_complete: true,
                target: SourceRevision::default(),
                base: SourceRevision::default(),
                model: ModelSelection { provider: "bedrock".into(), model_id: "m".into() },
                exclusions: None,
                review_notes: Vec::new(),
            },
            summary: "A change that does X and Y.".into(),
            decision: Decision::ReadyForReview,
            change_alignment: Some(ChangeAlignment {
                level: "aligned".into(),
                rationale: "Matches the PR goal.".into(),
            }),
            scores: Some(Scores {
                code_quality: Score {
                    score: 4,
                    rationale: "Tests do not cover the new path.".into(),
                    to_improve: "Add an integration test for the new path.".into(),
                },
                security: Score {
                    score: 5,
                    rationale: "No new external input.".into(),
                    to_improve: String::new(),
                },
            }),
            findings: Vec::new(),
            verification_gaps: Vec::new(),
            diagram: None,
            presentation_notes: Vec::new(),
        }
    }

    #[test]
    fn scores_render_as_stacked_blocks_not_a_table() {
        let md = markdown(&sample_report());
        assert!(md.contains("### Scores"));
        // Stacked, bold area + bar + value; no Markdown table pipes.
        assert!(md.contains("**Code quality** "));
        assert!(md.contains("4/5"));
        assert!(!md.contains("| Area |"));
        assert!(!md.contains("To reach 5/5"));
    }

    #[test]
    fn sub_five_score_emits_collapsible_ai_prompt_with_context_and_task() {
        let md = markdown(&sample_report());
        assert!(md.contains("<summary>AI prompt to reach 5/5 on Code quality</summary>"));
        // The prompt carries BOTH the why (context) and the improvement (task).
        assert!(md.contains("Context: Tests do not cover the new path."));
        assert!(md.contains("Task: Add an integration test for the new path."));
        // Security is 5/5, so it gets no prompt block.
        assert!(!md.contains("AI prompt to reach 5/5 on Security"));
    }

    #[test]
    fn footer_has_site_and_version_no_model_no_dash() {
        let md = markdown(&sample_report());
        assert!(md.contains("Reviewed by [Inkspect](https://inkspect.si)"));
        assert!(md.contains("engine v"));
        assert!(!md.contains('\u{2014}')); // no em dash
        assert!(!md.to_lowercase().contains("model:"));
    }

    #[test]
    fn one_presentation_contains_scores_diagram_and_safe_inline_comments() {
        let mut report = sample_report();
        report.summary = "<script>@everyone **fake**".into();
        report.run.review_notes.push(crate::config::ReviewNote { author: "@someone".into(), body: "Focus on validation".into() });
        report.diagram = Some(Diagram { title: "Flow".into(), view: "current".into(), evidence_ids: vec![],
            nodes: vec![DiagramNode { id: "injected-id".into(), label: "a\"] --> evil[\"b".into(), issue: false, evidence_ids: vec![] }], edges: vec![] });
        report.findings.push(Finding { id: "F-1".into(), title: "<b>@user</b>".into(), severity: Severity::Medium,
            reason: "A bug".into(), suggestion: "Fix it".into(), verification: "Check it".into(), advisory: None,
            revision: SourceRevision::default(), location: Location { path: "math.ts".into(), start_line: 3, end_line: 3 }, evidence_ids: vec![] });
        attach(&mut report);
        let p = report.presentation.as_ref().unwrap();
        assert_eq!(p.markdown, markdown(&report));
        assert_eq!(p.github_summary, p.markdown);
        assert!(p.markdown.contains("### Scores") && p.markdown.contains("```mermaid"));
        assert!(p.markdown.contains("Focus on validation"));
        assert!(!p.markdown.contains("<script>") && !p.markdown.contains("@everyone"));
        assert!(!p.markdown.contains("injected-id") && !p.markdown.contains("--> evil"));
        assert_eq!(p.github_findings["F-1"], finding_comment(&report.findings[0]));
        assert!(!p.github_findings["F-1"].contains("@user"));
        assert_eq!(p.fix_prompt, p.finding_prompts["F-1"]);
    }

    #[test]
    fn oversized_summary_preserves_full_report_and_never_claims_no_findings() {
        let mut report = sample_report();
        report.findings.push(Finding { id: "F-large".into(), title: "Large finding".into(), severity: Severity::Medium,
            reason: "界".repeat(30_000), suggestion: "Fix".into(), verification: "Test".into(), advisory: None,
            revision: SourceRevision::default(), location: Location { path: "math.ts".into(), start_line: 1, end_line: 1 }, evidence_ids: vec![] });
        attach(&mut report);
        let p = report.presentation.unwrap();
        assert!(p.markdown.len() > 58_000);
        assert!(p.github_summary.len() <= 58_000);
        assert!(p.github_summary.contains("omits findings"));
        assert!(!p.github_summary.contains("No findings on the changed lines"));
        assert!(p.github_findings["F-large"].len() <= 58_000);
    }
}
