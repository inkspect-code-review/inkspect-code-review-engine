//! Token-aware review of a pinned PR. One result is published after all parts succeed.
use std::collections::{HashMap, HashSet};

use crate::config::ReviewJob;
use crate::domain::{Decision, Report, Severity, SourceRevision, VerificationGap};
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
    ) -> String {
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
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    model: &ReviewModel,
    job: &ReviewJob,
    title: &str,
    body: &str,
    diff: &DiffSummary,
    retrieved: &[Retrieved],
    target: &SourceRevision,
    base: &SourceRevision,
) -> Result<Report> {
    let prompt = Prompt {
        nonce: prompts::new_nonce(),
        title,
        body,
        notes: json!(job.review_notes),
        job,
    };
    let system = prompts::system_prompt(&prompt.nonce);
    // Each successful review consumes at least one file or one hunk segment.
    // Record the upper bound before inference for crash-safe usage settlement.
    let maximum = diff
        .files
        .iter()
        .map(|f| 1 + f.hunk_text.lines().count())
        .sum::<usize>()
        .max(1);
    crate::telemetry::record(json!({"kind":"review.plan","parts_limit":maximum}))?;
    let limit = model.input_limit().await?;
    let mut report = if let Some(raw) =
        attempt(model, &prompt, &system, &diff.files, retrieved, None, limit).await?
    {
        verified_report(model, job, target, base, diff, raw)
    } else {
        let order = Links::new(&diff.files).order();
        let units: Vec<_> = order
            .into_iter()
            .flat_map(|i| fragments(&diff.files[i]))
            .collect();
        let links = Links::new(&units);
        let mut pending = Vec::new();
        let mut reports = Vec::new();
        if units.len() > 1 {
            let mid = split_point(&units);
            pending.extend([(mid, units.len()), (0, mid)]);
        } else {
            pending.push((0, units.len()));
        }
        let mut omitted = Vec::new();
        let mut missing_links = 0;
        while let Some((start, end)) = pending.pop() {
            let files = &units[start..end];
            let related = links.context(start, end);
            if let Some(raw) = attempt(
                model,
                &prompt,
                &system,
                files,
                retrieved,
                Some(&related),
                limit,
            )
            .await?
            {
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
                pending.extend([(mid, end), (start, mid)]);
            } else {
                // Try just this fragment when even the compact PR map is too large.
                if let Some(raw) = attempt(model, &prompt, &system, files, &[], None, limit).await?
                {
                    missing_links += 1;
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
                } else {
                    omitted.extend(files.iter().map(|f| f.path.clone()));
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

async fn attempt(
    model: &ReviewModel,
    prompt: &Prompt<'_>,
    system: &str,
    files: &[ChangedFile],
    mut retrieved: &[Retrieved],
    related: Option<&Value>,
    limit: Option<usize>,
) -> Result<Option<crate::model::RawReview>> {
    loop {
        let user = prompt.message(files, retrieved, related);
        let fits = match limit {
            Some(max) => model
                .count_input(system, &user)
                .await?
                .is_none_or(|n| n <= max),
            None => true,
        };
        if fits {
            match model.review(system, &user, prompt.job).await {
                Ok(raw) => return Ok(Some(raw)),
                Err(e) if e.reason == CONTEXT_LIMIT => {}
                Err(e) => return Err(e),
            }
        }
        if retrieved.is_empty() {
            return Ok(None);
        }
        retrieved = &[];
    }
}

// Partition very large text diffs on line boundaries, preserving both base and
// head coordinates in synthetic unified hunk headers. Never cut a UTF-8 line.
fn fragments(file: &ChangedFile) -> Vec<ChangedFile> {
    const PIECE: usize = 32 * 1024;
    if file.hunk_text.len() <= PIECE {
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
        if !block.is_empty() && block.len() + line.len() + 1 > PIECE {
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
    report.decision = if report.findings.iter().any(|f| f.severity != Severity::Low) {
        Decision::NeedsChanges
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
                let mut reader = BufReader::new(&stream);
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
        let result = run(&model, &job, "Change", "", &diff, &[], &target, &base).await;
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
        let parts = fragments(&f);
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
