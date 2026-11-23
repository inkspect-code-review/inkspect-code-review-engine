//! Token-aware review of a pinned PR. One result is published after all parts succeed.
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::config::{HostConfig, ReviewJob};
use crate::domain::{Decision, Report, SourceRevision, VerificationGap};
use crate::error::{err, Code, Result};
use crate::git::{ChangedFile, DiffSummary};
use crate::model::{ReviewModel, CONTEXT_LIMIT};
use crate::retrieval::Retrieved;
use crate::{prompts, review};
use serde_json::{json, Value};

/// Compare changed identifiers across the entire revision, including files in
/// later batches. Keep the matching source lines, so consumers and definitions
/// can be examined together regardless of which batch executes first.
struct Links<'a> {
    files: &'a [ChangedFile],
    words: Vec<HashSet<&'a str>>,
    frequency: HashMap<&'a str, usize>,
}

fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| word.len() >= 4 && word.chars().any(char::is_alphabetic))
}

impl<'a> Links<'a> {
    fn new(files: &'a [ChangedFile]) -> Self {
        let mut frequency = HashMap::new();
        let words = files
            .iter()
            .map(|file| {
                let set: HashSet<_> = identifiers(&file.hunk_text)
                    .chain(identifiers(&file.path))
                    .collect();
                for word in &set {
                    *frequency.entry(*word).or_default() += 1;
                }
                set
            })
            .collect();
        Self {
            files,
            words,
            frequency,
        }
    }

    /// Keep strongly connected definitions and callers close before partitioning.
    /// Posting lists are bounded by identifier frequency, so common language
    /// keywords cannot connect every file to every other file.
    fn order(&self) -> Vec<usize> {
        let threshold = (self.files.len() / 4).clamp(2, 16);
        let mut locations: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, words) in self.words.iter().enumerate() {
            for &word in words {
                if (2..=threshold).contains(&self.frequency[word]) {
                    locations.entry(word).or_default().push(i);
                }
            }
        }
        let mut remaining: std::collections::BTreeSet<usize> = (0..self.files.len()).collect();
        let mut next = 0;
        let mut order = Vec::with_capacity(remaining.len());
        while remaining.remove(&next) {
            order.push(next);
            let mut scores: HashMap<usize, usize> = HashMap::new();
            for word in &self.words[next] {
                if let Some(peers) = locations.get(word) {
                    for &peer in peers {
                        if remaining.contains(&peer) {
                            *scores.entry(peer).or_default() +=
                                word.len().min(32) * 16 / peers.len();
                        }
                    }
                }
            }
            let best = scores
                .into_iter()
                .max_by_key(|(id, score)| (*score, std::cmp::Reverse(*id)));
            let Some(id) = best
                .map(|(id, _)| id)
                .or_else(|| remaining.first().copied())
            else {
                break;
            };
            next = id;
        }
        order
    }

    fn context(&self, start: usize, end: usize) -> Value {
        let focus: HashSet<_> = self.words[start..end].iter().flatten().copied().collect();
        let mut excerpts = Vec::new();
        for (i, file) in self.files.iter().enumerate() {
            if (start..end).contains(&i) {
                continue;
            }
            // Prefer distinctive identifiers shared by a small number of files.
            let mut shared: Vec<_> = self.words[i]
                .intersection(&focus)
                .copied()
                .filter(|w| self.frequency[w] <= (self.files.len() / 4).clamp(2, 16))
                .collect();
            shared.sort_by_key(|w| (self.frequency[w], std::cmp::Reverse(w.len()), *w));
            shared.truncate(12);
            if shared.is_empty() {
                continue;
            }
            let mut lines = Vec::new();
            let mut anchor = "";
            for line in file.hunk_text.lines() {
                if line.starts_with("@@") {
                    anchor = line;
                    continue;
                }
                if !anchor.is_empty() && identifiers(line).any(|w| shared.contains(&w)) {
                    lines.push(json!([
                        anchor.chars().take(120).collect::<String>(),
                        line.chars().take(320).collect::<String>()
                    ]));
                    if lines.len() == 4 {
                        break;
                    }
                }
            }
            if !lines.is_empty() {
                excerpts.push((
                    self.frequency[shared[0]],
                    std::cmp::Reverse(shared.len()),
                    &file.path,
                    lines,
                ));
            }
        }
        excerpts.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
        json!({"changed_paths": self.files.iter().map(|f| &f.path).collect::<std::collections::BTreeSet<_>>(),
            "related_changes": excerpts.iter().take(24).map(|(_,_,path,lines)|json!({"path":path,"hunk_lines":lines})).collect::<Vec<_>>()})
    }
}

struct Prompt<'a> {
    scoped: Option<&'a crate::retrieval::pull::Context>,
    nonce: String,
    title: &'a str,
    body: &'a str,
    notes: Value,
    job: &'a ReviewJob,
}

impl Prompt<'_> {
    fn message(
        &self,
        files: &[ChangedFile],
        context: &[Retrieved],
        links: Option<&Value>,
        summaries: Option<&BTreeMap<&str, Value>>,
    ) -> String {
        let mut linked;
        let links = if let Some(scoped) = self.scoped {
            linked = links.cloned().unwrap_or_else(|| json!({}));
            linked["matching_evidence"] = json!(scoped.select(files));
            Some(&linked)
        } else { links };
        prompts::user_message_with_links(
            &self.nonce,
            self.title,
            self.body,
            files,
            files,
            context,
            &self.notes,
            &self.job.prior_findings,
            links,
            summaries,
        )
    }
}

struct RequestLimits<'a> {
    window: Option<usize>,
    budget: Option<&'a HostConfig>,
    remaining: u128,
}

impl<'a> RequestLimits<'a> {
    fn new(window: Option<usize>, host: &'a HostConfig) -> Self {
        let enforce = host.budget_nano_usd > 0 && host.best_effort_review_bytes.is_none();
        Self {
            window,
            budget: enforce.then_some(host),
            remaining: host.budget_nano_usd.into(),
        }
    }

    // None means shrink the input before requesting inference. A budget error
    // terminates the operation before it can authorize another paid call.
    fn reservation(&self, tokens: Option<usize>, bytes: usize) -> Result<Option<u128>> {
        if tokens
            .zip(self.window)
            .is_some_and(|(used, max)| used > max)
        {
            return Ok(None);
        }
        let Some(host) = self.budget else {
            return Ok(Some(0));
        };
        if host.model_input_allowance == 0 || host.model_max_output_tokens <= 0 {
            return Err(err(Code::Model, "invalid review budget allowance"));
        }
        // When token counting is unsupported, conservatively bound text input
        // by the UTF-8 request bytes plus the message envelope allowance.
        let input = tokens.unwrap_or(bytes) as u128;
        if bytes > host.model_max_request_bytes || input > u128::from(host.model_input_allowance) {
            return Ok(None);
        }
        let cost = input * u128::from(host.input_nano_usd)
            + host.model_max_output_tokens as u128 * u128::from(host.output_nano_usd);
        if cost > self.remaining {
            return Err(err(Code::ReviewBudget, "review inference budget exceeded"));
        }
        Ok(Some(cost))
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn run(
    model: &ReviewModel,
    host: &HostConfig,
    job: &ReviewJob,
    title: &str,
    body: &str,
    diff: &DiffSummary,
    retrieved: &[Retrieved],
    target: &SourceRevision,
    base: &SourceRevision,
) -> Result<Report> {
    run_with_context(model,host,job,title,body,diff,retrieved,target,base,None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_with_context(
    model: &ReviewModel,
    host: &HostConfig,
    job: &ReviewJob,
    title: &str,
    body: &str,
    diff: &DiffSummary,
    retrieved: &[Retrieved],
    target: &SourceRevision,
    base: &SourceRevision,
    scoped: Option<&crate::retrieval::pull::Context>,
) -> Result<Report> {
    let prompt = Prompt {
        scoped,
        nonce: prompts::new_nonce(),
        title,
        body,
        notes: json!(job.review_notes),
        job,
    };
    let system = prompts::system_prompt(&prompt.nonce);
    let summaries: BTreeMap<_, _> = if scoped.is_none() && job.prior_findings.is_empty() {
        diff.files
            .iter()
            .filter_map(|file| {
                crate::validation_json::summarize(file).map(|value| (file.path.as_str(), value))
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    // The optional summary pass and subsequent raw parts share one admission.
    let maximum = diff
        .files
        .iter()
        .map(|f| 1 + f.hunk_text.lines().count())
        .sum::<usize>()
        .max(1)
        + if summaries.is_empty() {
            0
        } else {
            diff.files.len()
        };
    crate::telemetry::record(json!({"kind":"review.plan","parts_limit":maximum}))?;
    let mut limits = RequestLimits::new(model.input_limit().await?, host);
    if !summaries.is_empty() {
        if let Some(report) = review_summaries(
            model,
            &prompt,
            &system,
            diff,
            &summaries,
            retrieved,
            target,
            base,
            &mut limits,
        )
        .await?
        {
            return Ok(report);
        }
    }
    let mut report = if let Some(raw) = attempt(
        model,
        &prompt,
        &system,
        &diff.files,
        retrieved,
        None,
        &mut limits,
    )
    .await?
    {
        verified_report(model, job, target, base, diff, raw)
    } else {
        let order = Links::new(&diff.files).order();
        let units: Vec<_> = order
            .into_iter()
            .flat_map(|i| fragments(&diff.files[i], 32 * 1024))
            .collect();
        let links = Links::new(&units);
        let mut pending = Vec::new();
        let mut reports = Vec::new();
        if units.len() > 1 {
            let mid = split_point(&units);
            pending.extend([(mid, units.len(), None), (0, mid, None)]);
        } else {
            pending.push((0, units.len(), None));
        }
        let mut omitted = Vec::new();
        let mut missing_links = 0;
        while let Some((start, end, piece)) = pending.pop() {
            let files = piece
                .as_ref()
                .map(std::slice::from_ref)
                .unwrap_or(&units[start..end]);
            let related = links.context(start, end);
            let mut raw = attempt(
                model,
                &prompt,
                &system,
                files,
                retrieved,
                Some(&related),
                &mut limits,
            )
            .await?;
            if raw.is_none() && files.len() == 1 {
                let bare = Prompt { scoped: None, nonce: prompt.nonce.clone(), notes: prompt.notes.clone(), ..prompt };
                raw = attempt(model, &bare, &system, files, &[], None, &mut limits).await?;
                if raw.is_some() && (units.len() > 1 || scoped.is_some()) {
                    missing_links += 1;
                }
            }
            if let Some(raw) = raw {
                reports.push(verified_report(
                    model,
                    job,
                    target,
                    base,
                    &DiffSummary {
                        files: files.to_vec(),
                    },
                    raw,
                ));
            } else if files.len() > 1 {
                let mid = start + split_point(files);
                pending.extend([(mid, end, None), (start, mid, None)]);
            } else if let [file] = files {
                // A long hunk header must not prevent even two short source
                // lines from being split after a provider rejection.
                let body_bytes = file
                    .hunk_text
                    .lines()
                    .skip_while(|line| !line.starts_with("@@ "))
                    .filter(|line| !line.starts_with("@@ "))
                    .map(|line| line.len() + 1)
                    .sum::<usize>();
                let parts = fragments(file, body_bytes / 2);
                if parts.len() > 1 {
                    pending.extend(parts.into_iter().rev().map(|p| (start, end, Some(p))));
                } else {
                    omitted.push(file.path.clone());
                }
            }
        }
        let mut merged = combine(reports)?;
        if missing_links > 0 {
            mark_gap(&mut merged,format!("Cross-file context exceeded the model window for {missing_links} change fragment(s)."));
        }
        if !omitted.is_empty() {
            mark_gap(
                &mut merged,
                format!(
                    "{} change fragment(s) exceeded the model's input limit: {}",
                    omitted.len(),
                    omitted.join(", ")
                ),
            );
        }
        merged
    };
    note_binary_files(&mut report, diff);
    Ok(report)
}

// Full PR diffs flag binary content with `unknown`. Its presence describes
// the scope of text review; omitted text fragments are tracked separately.
fn note_binary_files(report: &mut Report, diff: &DiffSummary) {
    let mut paths = diff
        .files
        .iter()
        .filter(|file| file.unknown)
        .map(|f| f.path.as_str());
    let listed: Vec<_> = paths.by_ref().take(5).collect();
    if listed.is_empty() {
        return;
    }
    let remaining = paths.count();
    let mut note = format!(
        "Binary content has no text diff to review: {}",
        listed.join(", ")
    );
    if remaining > 0 {
        note.push_str(&format!(" and {remaining} more"));
    }
    report.presentation_notes.push(note);
}

fn mark_gap(report: &mut Report, reason: String) {
    report.run.coverage_complete = false;
    report.verification_gaps.push(VerificationGap {
        reason,
        blocking: true,
    });
    if report.decision == Decision::ReadyForReview {
        report.decision = Decision::NeedsEvidence;
    }
}

fn verified_report(
    model: &ReviewModel,
    job: &ReviewJob,
    target: &SourceRevision,
    base: &SourceRevision,
    scope: &DiffSummary,
    raw: crate::model::RawReview,
) -> Report {
    let resolutions = review::verified_resolutions(&job.prior_findings, scope, &raw);
    let mut report = review::build_report(
        &job.run_id,
        target,
        base,
        model.provider(),
        model.model_id(),
        scope,
        raw,
    );
    report.resolved_findings = resolutions;
    report
}

// Keep complete JSON documents together across batches. A provisional concern
// in any batch discards the entire preview; the raw planner owns the final report.
#[allow(clippy::too_many_arguments)]
async fn review_summaries(
    model: &ReviewModel,
    prompt: &Prompt<'_>,
    system: &str,
    diff: &DiffSummary,
    summaries: &BTreeMap<&str, Value>,
    retrieved: &[Retrieved],
    target: &SourceRevision,
    base: &SourceRevision,
    limits: &mut RequestLimits<'_>,
) -> Result<Option<Report>> {
    let links = std::cell::OnceCell::new();
    let mut pending = vec![(0, diff.files.len())];
    let mut reports = Vec::new();
    while let Some((start, end)) = pending.pop() {
        let files = &diff.files[start..end];
        let related = (files.len() != diff.files.len()).then(|| {
            links
                .get_or_init(|| Links::new(&diff.files))
                .context(start, end)
        });
        let user = prompt.message(files, retrieved, related.as_ref(), Some(summaries));
        let Some(raw) = infer_if_fits(model, prompt.job, system, &user, limits).await? else {
            if files.len() == 1 {
                return Ok(None);
            }
            let mid = start + split_point(files);
            pending.extend([(mid, end), (start, mid)]);
            continue;
        };
        if !raw.findings.is_empty()
            || !raw.resolved_findings.is_empty()
            || !matches!(raw.decision.as_str(), "ready" | "ready_for_review")
        {
            return Ok(None);
        }
        reports.push(verified_report(model, prompt.job, target, base, diff, raw));
    }
    let mut report = combine(reports)?;
    report.presentation_notes.push(format!(
        "Reviewed {} added validation JSON file(s) using sampled values and field counts.",
        summaries.len()
    ));
    note_binary_files(&mut report, diff);
    Ok(Some(report))
}

async fn attempt(
    model: &ReviewModel,
    prompt: &Prompt<'_>,
    system: &str,
    files: &[ChangedFile],
    mut retrieved: &[Retrieved],
    related: Option<&Value>,
    limits: &mut RequestLimits<'_>,
) -> Result<Option<crate::model::RawReview>> {
    loop {
        let user = prompt.message(files, retrieved, related, None);
        if let Some(raw) = infer_if_fits(model, prompt.job, system, &user, limits).await? {
            return Ok(Some(raw));
        }
        if retrieved.is_empty() {
            return Ok(None);
        }
        retrieved = &[];
    }
}

async fn infer_if_fits(
    model: &ReviewModel,
    job: &ReviewJob,
    system: &str,
    user: &str,
    limits: &mut RequestLimits<'_>,
) -> Result<Option<crate::model::RawReview>> {
    let tokens = if limits.window.is_some() || limits.budget.is_some() {
        model.count_input(system, user).await?
    } else {
        None
    };
    let Some(cost) = limits.reservation(tokens, system.len() + user.len() + 4096)? else {
        return Ok(None);
    };
    limits.remaining -= cost;
    match model.review(system, user, job).await {
        Ok(raw) => Ok(Some(raw)),
        Err(e) if e.reason == CONTEXT_LIMIT => {
            limits.remaining += cost;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

// Partition very large text diffs on line boundaries, preserving both base and
// head coordinates in synthetic unified hunk headers. Never cut a UTF-8 line.
fn fragments(file: &ChangedFile, piece_bytes: usize) -> Vec<ChangedFile> {
    if file.hunk_text.len() <= piece_bytes {
        return vec![file.clone()];
    }
    hunk_fragments(file, piece_bytes)
}

pub(crate) fn hunk_fragments(file: &ChangedFile, piece_bytes: usize) -> Vec<ChangedFile> {
    if file.edits.is_empty() {
        return vec![file.clone()];
    }
    let mut parts = Vec::new();
    let mut base = 0;
    let mut head = 0;
    let mut block = String::new();
    let (mut removed, mut added) = (0, 0);
    let flush =
        |parts: &mut Vec<ChangedFile>, text: &mut String, bs: u32, hs: u32, bl: u32, hl: u32| {
            if text.is_empty() {
                return;
            }
            let edit = crate::git::LineEdit {
                base_start: bs,
                base_lines: bl,
                target_start: hs,
                target_lines: hl,
            };
            parts.push(ChangedFile {
                path: file.path.clone(),
                base_path: file.base_path.clone(),
                kind: file.kind.clone(),
                edits: vec![edit],
                unknown: file.unknown,
                hunk_text: format!("@@ -{bs},{bl} +{hs},{hl} @@\n{}", text),
                excerpt: String::new(),
            });
            text.clear();
        };
    let mut active = false;
    for line in file.hunk_text.lines() {
        if let Some(edit) = line
            .strip_prefix("@@ ")
            .and_then(crate::git::parse_hunk_header)
        {
            flush(&mut parts, &mut block, base, head, removed, added);
            base = edit.base_start;
            head = edit.target_start;
            removed = 0;
            added = 0;
            active = true;
            continue;
        }
        if !active {
            continue;
        }
        if !block.is_empty() && block.len() + line.len() + 1 > piece_bytes {
            flush(&mut parts, &mut block, base, head, removed, added);
            base += removed;
            head += added;
            removed = 0;
            added = 0;
        }
        if line.starts_with([' ', '-']) {
            removed += 1;
        }
        if line.starts_with([' ', '+']) {
            added += 1;
        }
        block.push_str(line);
        block.push('\n');
    }
    flush(&mut parts, &mut block, base, head, removed, added);
    if parts.is_empty() {
        parts.push(file.clone());
    }
    parts
}

/// Balance the source size, retaining repository order. Exact fit is checked
/// against the complete serialized messages on each side.
fn split_point(files: &[ChangedFile]) -> usize {
    let size = |f: &ChangedFile| f.hunk_text.len() + f.path.len() + 1;
    let half = files.iter().map(size).sum::<usize>() / 2;
    let mut used = 0;
    for (i, file) in files[..files.len() - 1].iter().enumerate() {
        used += size(file);
        if used >= half {
            return i + 1;
        }
    }
    files.len() - 1
}

fn combine(mut parts: Vec<Report>) -> Result<Report> {
    if parts.is_empty() {
        return Err(err(
            Code::Model,
            "no reviewable change fits the model context window",
        ));
    }
    let needs_changes = parts
        .iter()
        .any(|part| part.decision == Decision::NeedsChanges);
    let needs_evidence = parts
        .iter()
        .any(|part| part.decision == Decision::NeedsEvidence);
    let mut report = parts.remove(0);
    if parts.is_empty() {
        return Ok(report);
    }
    let count = 1 + parts.len();
    let mut summaries = vec![report.summary.clone()];
    for part in parts {
        summaries.push(part.summary);
        report.scores = match (report.scores.take(), part.scores) {
            (Some(mut scores), Some(other)) => {
                if other.security.score < scores.security.score {
                    scores.security = other.security;
                }
                if other.code_quality.score < scores.code_quality.score {
                    scores.code_quality = other.code_quality;
                }
                Some(scores)
            }
            _ => None,
        };
        report.run.coverage_complete &= part.run.coverage_complete;
        report.verification_gaps.extend(part.verification_gaps);
        report.findings.extend(part.findings);
        report.resolved_findings.extend(part.resolved_findings);
        report.presentation_notes.extend(part.presentation_notes);
    }
    report
        .findings
        .sort_by(|a, b| a.severity.cmp(&b.severity).then(a.id.cmp(&b.id)));
    let mut seen = HashSet::new();
    report.findings.retain(|f| seen.insert(f.id.clone()));
    let mut threads = HashMap::new();
    for r in &report.resolved_findings {
        *threads.entry(r.thread.clone()).or_insert(0) += 1;
    }
    report.resolved_findings.retain(|r| {
        threads[&r.thread] == 1
            && !report.findings.iter().any(|f| {
                f.location.path == r.path
                    && f.location.start_line <= r.line
                    && r.line <= f.location.end_line
            })
    });
    report.decision = if needs_changes {
        Decision::NeedsChanges
    } else if needs_evidence
        || !report.run.coverage_complete
        || report.verification_gaps.iter().any(|gap| gap.blocking)
    {
        Decision::NeedsEvidence
    } else {
        Decision::ReadyForReview
    };
    // Scores retain the weakest measured dimension across all parts. A diagram
    // or alignment assessment of a single part cannot describe the entire PR.
    report.diagram = None;
    report.change_alignment = None;
    report.summary = review::sanitize_text(&summaries.join("\n\n"), 4000);
    report.presentation_notes.push(format!(
        "Combined {count} review passes for the same revision."
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_bedrockruntime::{
        config::{Credentials, Region},
        Client, Config,
    };
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn host() -> HostConfig {
        serde_json::from_str(include_str!("../examples/host.example.json")).unwrap()
    }

    fn file(path: &str, code: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            base_path: path.into(),
            kind: "modified".into(),
            unknown: false,
            hunk_text: format!("@@ -1 +1 @@\n-old\n+{code}\n"),
            excerpt: code.into(),
            edits: vec![crate::git::LineEdit {
                base_start: 1,
                base_lines: 1,
                target_start: 1,
                target_lines: 1,
            }],
        }
    }

    fn read_request(stream: &std::net::TcpStream) -> (String, Value) {
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut size = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
            if let Some(length) = header.to_lowercase().strip_prefix("content-length:") {
                size = length.trim().parse().unwrap();
            }
        }
        let mut bytes = vec![0; size];
        reader.read_exact(&mut bytes).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        (request, body)
    }

    // Exercise the actual CountTokens and Converse wire formats through a local
    // SDK endpoint. Only the synthetic counter decides whether batching is needed.
    async fn exercise(split: bool, fail_second: bool, native_validation: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let file_count = if split { 2 } else { 96 };
        let server = std::thread::spawn(move || {
            let mut reviewed = Vec::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let (request, body) = read_request(&stream);
                let counting = request.contains("/count-tokens ");
                let conv = if counting {
                    &body["input"]["converse"]
                } else {
                    &body
                };
                let prompt = conv["messages"][0]["content"][0]["text"].as_str().unwrap();
                let payload: Value = serde_json::from_str(prompt.lines().nth(1).unwrap()).unwrap();
                let files = payload["untrusted_changed_source"].as_array().unwrap();
                let paths: Vec<_> = files
                    .iter()
                    .map(|f| f["path"].as_str().unwrap().to_owned())
                    .collect();
                let mut status = "200 OK";
                let response=if counting && native_validation {
                    status="400 Bad Request";
                    json!({"__type":"ValidationException","message":"token counting unsupported"})
                } else if !counting && native_validation && paths.len()>1 {
                    status="400 Bad Request";
                    json!({"__type":"ValidationException","message":"too many input tokens"})
                } else if counting {
                    json!({"inputTokens": if split && paths.len()>1 {923000} else {1000}})
                } else {
                    if !split {assert_eq!(paths.len(),96); assert!(prompt.len()>1_500_000);}
                    if split {
                        let related=&payload["untrusted_cross_batch_context"]["related_changes"];
                        let peer=if paths[0]=="provider.rs" {"consumer.rs"} else {"provider.rs"};
                        assert_eq!(related[0]["path"],peer);
                        assert!(related[0]["hunk_lines"].to_string().contains("OrderContract"));
                    }
                    let finding=json!({"severity":"P2","title":"Contract mismatch","reason":"Synthetic mismatch",
                        "suggestion":"Update caller","verification":"Check both ends","path":paths[0],"start_line":1,"end_line":1});
                    reviewed.extend(paths);
                    if fail_second && reviewed.len()==2 {status="500 Internal Server Error";json!({"message":"inference status unknown"})}
                    else {json!({"output":{"message":{"role":"assistant","content":[{"text":
                        json!({"summary":"Review complete","decision":"needs_changes","findings":[finding]}).to_string()}]}},
                        "stopReason":"end_turn","usage":{"inputTokens":50,"outputTokens":10,"totalTokens":60},"metrics":{"latencyMs":1}})}
                }.to_string();
                write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
                if reviewed.len() == file_count {
                    break;
                }
            }
            reviewed
        });
        let client = Client::from_conf(
            Config::builder()
                .behavior_version_latest()
                .region(Region::new("us-east-1"))
                .retry_config(aws_config::retry::RetryConfig::disabled())
                .credentials_provider(Credentials::new("fixture", "fixture", None, None, "test"))
                .endpoint_url(format!("http://{address}"))
                .build(),
        );
        let model = ReviewModel::with_test_client(client);
        let job: ReviewJob =
            serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
        let mut diff = DiffSummary {
            files: vec![
                file("provider.rs", "fn OrderContract() {}"),
                file("consumer.rs", "let x = OrderContract();"),
            ],
        };
        if !split {
            let code = "safe_sample();".repeat(1300);
            for n in 2..file_count {
                diff.files.push(file(&format!("sample-{n}.rs"), &code));
            }
        }
        let target = job.target_revision();
        let mut base = target.clone();
        base.revision_id = job.base_sha.clone();
        let result = run(
            &model,
            &host(),
            &job,
            "Change",
            "",
            &diff,
            &[],
            &target,
            &base,
        )
        .await;
        if fail_second {
            assert!(result.is_err());
        } else {
            let report = result.unwrap();
            assert!(report.run.coverage_complete);
            assert_eq!(report.findings.len(), if split { 2 } else { 1 });
            assert_eq!(report.decision, Decision::NeedsChanges);
        }
        assert_eq!(
            server.join().unwrap(),
            diff.files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
        );
    }

    // The synthetic token counter accepts at most one line. Record every
    // inference request (not count requests) to check coverage and spending.
    async fn run_small_hunk(budget: u64) -> (Result<Report>, Vec<u32>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for connection in listener.incoming() {
                let mut stream = connection.unwrap();
                let mut first = [0; 1];
                if stream.peek(&mut first).unwrap() == 0 {
                    break;
                }
                let (request, body) = read_request(&stream);
                let counting = request.contains("/count-tokens ");
                let conversation = if counting {
                    &body["input"]["converse"]
                } else {
                    &body
                };
                let prompt = conversation["messages"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap();
                let payload: Value = serde_json::from_str(prompt.lines().nth(1).unwrap()).unwrap();
                let hunk = payload["untrusted_changed_source"][0]["hunk"]
                    .as_str()
                    .unwrap();
                let edit = crate::git::parse_hunk_header(
                    hunk.lines().next().unwrap().trim_start_matches("@@ "),
                )
                .unwrap();
                let response = if counting {
                    json!({"inputTokens":if edit.target_lines > 1 {923_000} else {1000}})
                } else {
                    assert_eq!(edit.target_lines, 1);
                    seen.push(edit.target_start);
                    assert_eq!(hunk.lines().nth(1), Some(format!("+line{}", edit.target_start).as_str()));
                    json!({"output":{"message":{"role":"assistant","content":[{"text":r#"{"summary":"Reviewed","decision":"ready","findings":[]}"#}]}},
                        "stopReason":"end_turn","usage":{"inputTokens":1000,"outputTokens":10,"totalTokens":1010},"metrics":{"latencyMs":1}})
                }.to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
            }
            seen
        });
        let config = Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("fixture", "fixture", None, None, "test"))
            .retry_config(aws_config::retry::RetryConfig::disabled())
            .endpoint_url(format!("http://{address}"))
            .build();
        let model = ReviewModel::with_test_client(Client::from_conf(config));
        let mut host = host();
        host.model_max_output_tokens = 4096;
        host.model_input_allowance = 922_000;
        host.model_max_request_bytes = 256 * 1024;
        host.input_nano_usd = 1;
        host.output_nano_usd = 1;
        host.budget_nano_usd = budget;
        let job: ReviewJob =
            serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
        let target = job.target_revision();
        let mut file = file("sample.rs", "");
        file.hunk_text = "@@ -7,0 +8,4 @@ change\n+line8\n+line9\n+line10\n+line11\n".into();
        file.edits = vec![crate::git::parse_hunk_header("-7,0 +8,4 @@").unwrap()];
        let diff = DiffSummary { files: vec![file] };
        let result = run(
            &model,
            &host,
            &job,
            "Change",
            "",
            &diff,
            &[],
            &target,
            &target,
        )
        .await;
        // Closing an empty connection terminates the fixture even on a budget rejection.
        drop(std::net::TcpStream::connect(address).unwrap());
        (result, server.join().unwrap())
    }

    #[tokio::test]
    async fn small_rejected_fragment_is_subdivided_until_every_line_fits() {
        let (result, seen) = run_small_hunk(4 * 5096).await;
        let report = result.unwrap();
        assert_eq!(seen, [8, 9, 10, 11]);
        assert!(report.run.coverage_complete && report.verification_gaps.is_empty());
    }

    #[tokio::test]
    async fn insufficient_operation_budget_prevents_unauthorized_inference() {
        for (budget, expected) in [(5095, vec![]), (2 * 5096 - 1, vec![8])] {
            let (result, seen) = run_small_hunk(budget).await;
            assert_eq!(
                result.unwrap_err().code.as_str(),
                "review.budget_limit"
            );
            assert_eq!(seen, expected);
        }
    }

    #[tokio::test]
    async fn validation_summary_never_publishes_preliminary_findings() {
        for (preliminary, budget, expected_calls) in [
            ("ready", 5096, 1),
            ("candidate", 10192, 2),
            ("needs_evidence", 10192, 2),
            ("candidate", 5096, 1),
            ("split", 10192, 2),
            ("confirmed", 10192, 2),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let mut calls = 0;
                for connection in listener.incoming() {
                    let mut stream = connection.unwrap();
                    if stream.peek(&mut [0]).unwrap() == 0 {
                        break;
                    }
                    let (request, body) = read_request(&stream);
                    let response = if request.contains("/count-tokens ") {
                        let msg=body["input"]["converse"]["messages"][0]["content"][0]["text"].as_str().unwrap();
                        let payload:Value=serde_json::from_str(msg.lines().nth(1).unwrap()).unwrap();
                        let split=preliminary=="split" && payload["changes"].as_array().unwrap().len()>1;
                        json!({"inputTokens": if split {923000} else {1000}})
                    } else {
                        calls += 1;
                        let user = body["messages"][0]["content"][0]["text"].as_str().unwrap();
                        let payload: Value = serde_json::from_str(user.lines().nth(1).unwrap()).unwrap();
                        let source = &payload["untrusted_changed_source"][0];
                        assert_eq!(payload["untrusted_retrieved_context"][0]["content"],"same source");
                        let raw = if calls == 1 || preliminary == "split" {
                            assert_eq!(source["validation_summary"]["passed_true"], 900);
                            assert!(source.get("hunk").is_none());
                            let findings = if matches!(preliminary, "candidate" | "confirmed") {
                                json!([{"severity":"P1","title":"Unconfirmed","reason":"Unconfirmed","suggestion":"Check","verification":"Check","path":"docs/validation/results.json","start_line":1,"end_line":1}])
                            } else {json!([])};
                            json!({"summary":"Sampled","decision":if preliminary=="split" {"ready"} else {preliminary},"findings":findings})
                        } else {
                            assert!(source["hunk"].as_str().unwrap().contains("+    \"passed\": true"));
                            assert!(source.get("validation_summary").is_none());
                            let findings = if preliminary=="confirmed" {
                                json!([{"severity":"P1","title":"Raw source finding","reason":"Original rows checked","suggestion":"Check","verification":"Check","path":"docs/validation/results.json","start_line":3,"end_line":3}])
                            } else {json!([])};
                            json!({"summary":"Reviewed original source","decision":"ready","findings":findings})
                        };
                        json!({"output":{"message":{"role":"assistant","content":[{"text":raw.to_string()}]}},
                          "stopReason":"end_turn","usage":{"inputTokens":1000,"outputTokens":10,"totalTokens":1010},"metrics":{"latencyMs":1}})
                    }.to_string();
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
                }
                calls
            });
            let config = Config::builder()
                .behavior_version_latest()
                .region(Region::new("us-east-1"))
                .credentials_provider(Credentials::new("fixture", "fixture", None, None, "test"))
                .retry_config(aws_config::retry::RetryConfig::disabled())
                .endpoint_url(format!("http://{address}"))
                .build();
            let model = ReviewModel::with_test_client(Client::from_conf(config));
            let mut host = host();
            host.model_max_output_tokens = 4096;
            host.model_input_allowance = 922_000;
            host.model_max_request_bytes = 256 * 1024;
            host.input_nano_usd = 1;
            host.output_nano_usd = 1;
            host.budget_nano_usd = budget;
            let job: ReviewJob =
                serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
            let raw = serde_json::to_string_pretty(&vec![json!({"passed":true}); 900]).unwrap();
            let mut diff = DiffSummary {
                files: vec![crate::validation_json::fixture(&raw)],
            };
            if preliminary == "split" {
                let mut second = diff.files[0].clone();
                second.path = "docs/validation/other.json".into();
                diff.files.push(second);
            }
            let retrieved = vec![Retrieved {
                path: "src/check.rs".into(),
                start_line: 1,
                end_line: 1,
                content: "same source".into(),
            }];
            let target = job.target_revision();
            let result = run(
                &model,
                &host,
                &job,
                "New results",
                "",
                &diff,
                &retrieved,
                &target,
                &target,
            )
            .await;
            drop(std::net::TcpStream::connect(address).unwrap());
            assert_eq!(server.join().unwrap(), expected_calls);
            if preliminary == "candidate" && expected_calls == 1 {
                assert_eq!(result.unwrap_err().code.as_str(), "review.budget_limit");
            } else {
                let report = result.unwrap();
                if preliminary == "confirmed" {
                    assert_eq!(report.findings.len(), 1);
                    assert_eq!(report.findings[0].title, "Raw source finding");
                } else {
                    assert!(report.findings.is_empty());
                }
                if expected_calls == 2 && preliminary != "split" {
                    assert_eq!(report.summary, "Reviewed original source");
                } else {
                    assert!(report
                        .presentation_notes
                        .iter()
                        .any(|n| n.contains("sampled values")));
                }
            }
        }
    }

    #[tokio::test]
    async fn full_input_fits_one_inference() {
        exercise(false, false, false).await;
    }
    #[tokio::test]
    async fn split_preserves_cross_file_evidence_and_combines_results() {
        exercise(true, false, false).await;
    }
    #[tokio::test]
    async fn uncertain_second_call_fails_without_replay_or_partial_publication() {
        exercise(true, true, false).await;
    }

    #[tokio::test]
    async fn unsupported_counter_uses_definitive_provider_validation() {
        exercise(true, false, true).await;
    }

    #[tokio::test]
    async fn unknown_openai_model_splits_only_after_a_context_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut provider = crate::byok::Provider::decode(
            &serde_json::to_vec(&json!({
                "provider": "openai", "model": "deployment-alias",
                "api_key": "x".repeat(16), "max_output_tokens": 4096
            }))
            .unwrap(),
        )
        .unwrap();
        provider.test_port = Some(port);
        let model = ReviewModel::from_provider(provider, 4096);
        let server = std::thread::spawn(move || {
            let mut batches = Vec::new();
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let (request, body) = read_request(&stream);
                assert!(request.starts_with("POST /v1/responses "));
                assert_eq!(body["model"], "deployment-alias");
                let input: Value =
                    serde_json::from_str(body["input"].as_str().unwrap().lines().nth(1).unwrap())
                        .unwrap();
                let paths: Vec<_> = input["untrusted_changed_source"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|file| file["path"].as_str().unwrap().to_owned())
                    .collect();
                let (status, response) = if paths.len() > 1 {
                    (
                        "400 Bad Request",
                        json!({"error":{"code":"context_length_exceeded"}}),
                    )
                } else {
                    (
                        "200 OK",
                        json!({"status":"completed","output":[{"type":"message","role":"assistant",
                        "content":[{"type":"output_text","text":r#"{"summary":"Reviewed","decision":"ready","findings":[]}"#}]}]}),
                    )
                };
                batches.push(paths);
                let data = response.to_string();
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).unwrap();
            }
            batches
        });
        let job: ReviewJob =
            serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
        let target = job.target_revision();
        let diff = DiffSummary {
            files: vec![file("a.rs", "alpha"), file("b.rs", "beta")],
        };
        let report = run(
            &model,
            &host(),
            &job,
            "Change",
            "",
            &diff,
            &[],
            &target,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(report.decision, Decision::ReadyForReview);
        assert!(report.run.coverage_complete);
        assert_eq!(
            server.join().unwrap(),
            [vec!["a.rs", "b.rs"], vec!["a.rs"], vec!["b.rs"]]
        );
    }

    #[test]
    fn combining_reports_preserves_blocking_gaps_and_verdict_precedence_in_both_orders() {
        let job: ReviewJob =
            serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
        let target = job.target_revision();
        let scope = DiffSummary {
            files: vec![file("a.rs", "alpha")],
        };
        let raw = crate::model::parse_review(
            r#"{"summary":"Reviewed","decision":"ready","findings":[]}"#,
        )
        .unwrap();
        let ready =
            review::build_report(&job.run_id, &target, &target, "test", "test", &scope, raw);
        let mut incomplete = ready.clone();
        mark_gap(&mut incomplete, "Missing source".into());
        for decision in [Decision::ReadyForReview, Decision::NeedsChanges] {
            let mut other = ready.clone();
            other.decision = decision;
            for parts in [
                vec![incomplete.clone(), other.clone()],
                vec![other.clone(), incomplete.clone()],
            ] {
                let combined = combine(parts).unwrap();
                let expected = if decision == Decision::NeedsChanges {
                    decision
                } else {
                    Decision::NeedsEvidence
                };
                assert_eq!(combined.decision, expected);
                assert!(!combined.run.coverage_complete);
                assert_eq!(combined.verification_gaps.len(), 1);
                assert!(combined.verification_gaps[0].blocking);
                assert_eq!(combined.verification_gaps[0].reason, "Missing source");
            }
        }
    }

    #[test]
    fn binary_content_is_disclosed_without_changing_the_review_verdict() {
        let job: ReviewJob =
            serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
        let target = job.target_revision();
        let mut fixture = file("fixtures/reference.xz", "");
        fixture.unknown = true;
        let scope = DiffSummary {
            files: vec![file("source.rs", "fn example() {}"), fixture],
        };
        let raw = crate::model::parse_review(
            r#"{"summary":"Reviewed","decision":"ready","findings":[]}"#,
        )
        .unwrap();
        let report =
            review::build_report(&job.run_id, &target, &target, "test", "test", &scope, raw);
        for (decision, incomplete) in [
            (Decision::ReadyForReview, false),
            (Decision::NeedsChanges, false),
            (Decision::NeedsEvidence, true),
        ] {
            let mut result = report.clone();
            result.decision = decision;
            if incomplete {
                mark_gap(&mut result, "Missing text fragment".into());
            }
            note_binary_files(&mut result, &scope);
            assert_eq!(result.decision, decision);
            assert_eq!(result.run.coverage_complete, !incomplete);
            assert_eq!(
                result.verification_gaps.iter().any(|gap| gap.blocking),
                incomplete
            );
            assert_eq!(
                result.presentation_notes,
                ["Binary content has no text diff to review: fixtures/reference.xz"]
            );
        }
    }

    #[test]
    fn large_file_fragments_keep_unicode_and_head_coordinates() {
        let mut f = file("big.rs", "");
        let text = "let key = \"ş\";".repeat(3000);
        f.hunk_text = format!("@@ -20,1 +20,3 @@\n-old\n+a\n+{text}\n+z\n");
        let parts = fragments(&f, 32 * 1024);
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts
                .iter()
                .map(|p| p.edits[0].target_start)
                .collect::<Vec<_>>(),
            [20, 21, 22]
        );
        assert!(parts[1].hunk_text.contains(&text));
    }
}
