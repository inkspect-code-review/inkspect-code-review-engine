//! Compact evidence for large, newly added validation results. Original hunks
//! stay in the review plan for independent verification.
use std::collections::{BTreeMap, BTreeSet};

use crate::git::{selected_lines, ChangedFile};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Default)]
struct Field<'a> {
    count: usize,
    example: Option<&'a Value>,
    min: Option<f64>,
    max: Option<f64>,
    true_count: usize,
    false_count: usize,
}

#[derive(Default)]
struct Profile<'a> {
    arrays: usize,
    objects: usize,
    members: usize,
    passed: usize,
    passed_case_count: Option<u64>,
    crc_checks: usize,
    digests: usize,
    checksum_sequences: BTreeMap<&'a str, Sha256>,
    fields: BTreeMap<&'a str, Field<'a>>,
    // Keep non-checksum text associated with its field, even for rare fields.
    text: BTreeMap<&'a str, BTreeSet<&'a str>>,
    text_bytes: usize,
}

impl<'a> Profile<'a> {
    fn visit(&mut self, name: &'a str, value: &'a Value, text_limit: usize) -> Option<()> {
        // Keep failed or malformed checks in the raw review, including
        // their surrounding record and its original line numbers.
        match name {
            "passed" if value.as_bool() == Some(true) => self.passed += 1,
            "crc_bad_frames" if value.as_u64() == Some(0) => self.crc_checks += 1,
            "passed" | "crc_bad_frames" => return None,
            _ => {}
        }
        match value {
            Value::Object(obj) => {
                self.objects += 1;
                self.members += obj.len();
                for (name, child) in obj {
                    self.visit(name, child, text_limit)?;
                }
            }
            Value::Array(items) => {
                self.arrays += 1;
                for child in items {
                    self.visit(name, child, text_limit)?;
                }
            }
            scalar => {
                let field = self.fields.entry(name).or_default();
                field.count += 1;
                field.example.get_or_insert(scalar);
                if scalar.as_u64().is_some_and(|v| v > (1 << 53))
                    || scalar.as_i64().is_some_and(|v| v < -(1 << 53))
                {
                    return None;
                }
                if let Some(n) = scalar.as_f64() {
                    field.min = Some(field.min.map_or(n, |v| v.min(n)));
                    field.max = Some(field.max.map_or(n, |v| v.max(n)));
                }
                match scalar {
                    Value::Bool(true) => field.true_count += 1,
                    Value::Bool(false) => field.false_count += 1,
                    Value::String(s)
                        if name.ends_with("sha256")
                            && s.len() == 64
                            && s.bytes().all(|b| b.is_ascii_hexdigit()) =>
                    {
                        self.digests += 1;
                        self.checksum_sequences
                            .entry(name)
                            .or_default()
                            .update(s.as_bytes());
                    }
                    Value::String(s) => {
                        if self.text.entry(name).or_default().insert(s) {
                            self.text_bytes += s.len();
                            if self.text_bytes > text_limit {
                                return None;
                            }
                        }
                    }
                    _ => {}
                }
                if self.fields.len() > 1024 {
                    return None;
                }
            }
        }
        Some(())
    }
}

// serde_json keeps the final value for a duplicate key. Count members in the
// original JSON too, so duplicate-key documents always receive a raw review.
fn member_count(text: &str) -> usize {
    let (mut quoted, mut escaped, mut count) = (false, false, 0);
    for byte in text.bytes() {
        if escaped {
            escaped = false;
        } else if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && byte == b':' {
            count += 1;
        }
    }
    count
}

pub(super) fn summarize(file: &ChangedFile) -> Option<Value> {
    if file.unknown
        || file.kind != "added"
        || !file.path.starts_with("docs/validation/")
        || !file.path.ends_with(".json")
        || !(8192..=2_097_152).contains(&file.hunk_text.len())
    {
        return None;
    }
    let [edit] = file.edits.as_slice() else {
        return None;
    };
    if edit.base_lines != 0 || edit.target_start != 1 {
        return None;
    }
    let text = selected_lines(&file.hunk_text, 1, edit.target_lines)?.join("\n");
    let doc: Value = serde_json::from_str(&text).ok()?;
    let mut profile = Profile::default();
    let limit = file.hunk_text.len() / 2;
    // Some reports use an integer roll-up alongside an array or map of cases.
    // Preserve that count distinctly from boolean observations.
    if let Some(count) = doc.get("passed").and_then(Value::as_u64) {
        let cases = match doc.get("cases")? {
            Value::Array(items) => items.len(),
            Value::Object(items) => items.len(),
            _ => return None,
        };
        if count != cases as u64 || doc.get("total").is_some_and(|v| v.as_u64() != Some(count)) {
            return None;
        }
        profile.passed_case_count = Some(count);
        let obj = doc.as_object()?;
        profile.objects += 1;
        profile.members += obj.len();
        for (key, child) in obj {
            if key != "passed" {
                profile.visit(key, child, limit)?;
            }
        }
    } else {
        profile.visit("", &doc, limit)?;
    }
    if profile.members != member_count(&text)
        || profile.passed + profile.crc_checks + profile.digests == 0
    {
        return None;
    }
    let mut fields: Vec<_> = profile.fields.into_iter().collect();
    fields.sort_by_key(|(name, f)| (std::cmp::Reverse(f.count), *name));
    let samples: Vec<_> = fields
        .iter()
        .take(12)
        .map(|(name, f)| {
            let mut sample = json!({"field":name,"count":f.count,"example":f.example});
            if f.min.is_some() {
                sample["numeric_range"] = json!([f.min, f.max]);
            }
            if f.true_count + f.false_count > 0 {
                sample["true_false"] = json!([f.true_count, f.false_count]);
            }
            sample
        })
        .collect();
    let checksum_sequences: BTreeMap<_, _> = profile
        .checksum_sequences
        .into_iter()
        .map(|(key, hasher)| (key, hex::encode(hasher.finalize())))
        .collect();
    let summary = json!({"objects":profile.objects,"arrays":profile.arrays,
        "field_counts":fields.iter().map(|(name,f)| (*name,f.count)).collect::<BTreeMap<_,_>>(),
        "passed_case_count":profile.passed_case_count,"passed_true":profile.passed,"zero_crc_bad_frames":profile.crc_checks,"sha256_values":profile.digests,
        "sha256_sequence_fingerprints":checksum_sequences,"sampled_fields":samples,"distinct_text_by_field":profile.text});
    (summary.to_string().len() < limit).then_some(summary)
}

#[cfg(test)]
pub(crate) fn fixture(body: &str) -> ChangedFile {
    let lines = body.lines().count();
    ChangedFile {
        kind: "added".into(),
        path: "docs/validation/results.json".into(),
        base_path: String::new(),
        unknown: false,
        edits: vec![crate::git::LineEdit {
            base_start: 0,
            base_lines: 0,
            target_start: 1,
            target_lines: lines as u32,
        }],
        hunk_text: format!(
            "@@ -0,0 +1,{lines} @@\n{}",
            body.lines().map(|s| format!("+{s}\n")).collect::<String>()
        ),
        excerpt: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> String {
        let records: Vec<_> = (0..300)
            .map(|n| json!({"passed":true,"crc_bad_frames":0,"frames":n,"unit":"frames"}))
            .collect();
        serde_json::to_string_pretty(&records).unwrap()
    }

    #[test]
    fn counts_every_record_and_preserves_uncommon_text() {
        let mut body = data();
        body = body.replacen(
            "\"frames\": 299",
            "\"frames\": 299, \"note\": \"unusual observation\"",
            1,
        );
        let f = fixture(&body);
        let s = summarize(&f).unwrap();
        assert_eq!(s["objects"], 300);
        assert_eq!(s["passed_true"], 300);
        assert_eq!(s["zero_crc_bad_frames"], 300);
        assert_eq!(
            s["distinct_text_by_field"]["note"],
            json!(["unusual observation"])
        );
        assert_eq!(
            s["sampled_fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["field"] == "frames")
                .unwrap()["numeric_range"],
            json!([0.0, 299.0])
        );
        assert!(s.to_string().len() < f.hunk_text.len() / 5);
    }

    #[test]
    fn distinguishes_pass_counts_from_flags_and_keeps_mismatches_raw() {
        let cases = vec![json!({"passed":true,"frames":20}); 500];
        let mut doc = json!({"cases":cases,"passed":500,"total":500});
        let summary = summarize(&fixture(&serde_json::to_string_pretty(&doc).unwrap())).unwrap();
        assert_eq!(summary["passed_case_count"], 500);
        assert_eq!(summary["passed_true"], 500);
        doc["total"] = json!(501);
        assert!(summarize(&fixture(&serde_json::to_string_pretty(&doc).unwrap())).is_none());
        doc["passed"] = json!(false);
        assert!(summarize(&fixture(&serde_json::to_string_pretty(&doc).unwrap())).is_none());
    }

    #[test]
    fn a_single_changed_checksum_changes_its_field_fingerprint() {
        let rows = vec![json!({"output_sha256":"a".repeat(64)}); 150];
        let mut changed = rows.clone();
        changed[137]["output_sha256"] = json!("b".repeat(64));
        let before = summarize(&fixture(&serde_json::to_string_pretty(&rows).unwrap())).unwrap();
        let after = summarize(&fixture(&serde_json::to_string_pretty(&changed).unwrap())).unwrap();
        assert_ne!(
            before["sha256_sequence_fingerprints"]["output_sha256"],
            after["sha256_sequence_fingerprints"]["output_sha256"]
        );
    }

    #[test]
    fn abnormal_ambiguous_and_incomplete_inputs_keep_their_source() {
        let body = data();
        for (from, to) in [
            ("\"passed\": true", "\"passed\": false"),
            ("\"crc_bad_frames\": 0", "\"crc_bad_frames\": 1"),
            ("\"passed\": true", "\"passed\": 42"),
            ("\"passed\": true", "\"passed\": false, \"passed\": true"),
            ("299", "???"),
        ] {
            assert!(summarize(&fixture(&body.replacen(from, to, 1))).is_none());
        }
        let mut file = fixture(&body);
        file.edits[0].target_lines += 1;
        assert!(summarize(&file).is_none());
        for path in ["src/results.json", "docs/validation/results.js"] {
            let mut f = fixture(&body);
            f.path = path.into();
            assert!(summarize(&f).is_none());
        }
        let mut file = fixture(&body);
        file.kind = "modified".into();
        assert!(summarize(&file).is_none());
        assert!(summarize(&fixture("{}")).is_none());
    }
}
