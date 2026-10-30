//! Private, exact model inputs. Never put source context in application logs.
use std::io::Write;
use std::path::Path;

use crate::config::ReviewJob;
use crate::error::{err, Code, Result};

pub fn capture(job: &ReviewJob, model: &str, region: &str, max_tokens: i32, system: &str, user: &str) -> Result<()> {
    let Some(path) = std::env::var_os("INKSPECT_REVIEW_CONTEXT_FILE") else { return Ok(()); };
    write_private(Path::new(&path), &document(job, model, region, max_tokens, system, user))
}

fn document(job: &ReviewJob, model: &str, region: &str, max_tokens: i32, system: &str, user: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "stage": "prepared_before_send",
        "engine": "rust",
        "engine_version": crate::render::ENGINE_VERSION,
        "prompt_version": crate::prompts::PROMPT_VERSION,
        "run_id": job.run_id,
        "base": job.base_sha,
        "head": job.head_sha,
        "repository_id": job.repository_id,
        "tenant_id": job.tenant_id,
        "provider": "bedrock",
        "region": region,
        "request": {
            "modelId": model,
            "system": [{"text": system}],
            "messages": [{"role": "user", "content": [{"text": user}]}],
            "inferenceConfig": {"maxTokens": max_tokens}
        }
    })
}

fn write_private(path: &Path, value: &serde_json::Value) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let write = || -> std::io::Result<()> {
        // NamedTempFile is owner-only. Publish atomically and never replace an
        // earlier request if the same operation is accidentally replayed.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path).map_err(|e| e.error)?;
        std::fs::File::open(parent)?.sync_all()
    };
    write().map_err(|_| err(Code::Internal, "review context could not be saved"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_the_complete_prompt_payload_and_model_parameters() {
        let job: ReviewJob = serde_json::from_str(include_str!("../examples/job.example.json")).unwrap();
        let nonce = crate::prompts::new_nonce();
        let system = crate::prompts::system_prompt(&nonce);
        let rag = vec![crate::retrieval::Retrieved { path: "math.ts".into(), start_line: 3, end_line: 4, content: "// GİT\nexport const n = 42;".into() }];
        let notes = serde_json::to_value(&job.review_notes).unwrap();
        let user = crate::prompts::user_message(&nonce, "Add math", "PR intent", &crate::git::DiffSummary { files: vec![] }, &[], &rag, &notes);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-context.json");
        write_private(&path, &document(&job, "model-id", "eu-west-2", 2048, &system, &user)).unwrap();
        let captured: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(captured["request"]["system"][0]["text"], system);
        assert_eq!(captured["request"]["messages"][0]["content"][0]["text"], user);
        assert_eq!(captured["request"]["modelId"], "model-id");
        assert_eq!(captured["request"]["inferenceConfig"]["maxTokens"], 2048);
        assert_eq!(captured["run_id"], job.run_id);
        let payload: serde_json::Value = serde_json::from_str(user.lines().nth(1).unwrap()).unwrap();
        assert_eq!(payload["untrusted_retrieved_context"][0]["content"], rag[0].content);
        assert_eq!(payload["untrusted_review_notes"], notes);
    }

    #[test]
    fn preserves_exact_unicode_rag_and_nonce_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-context.json");
        let user = "nonce\n{\"untrusted_retrieved_context\":[{\"content\":\"GİT\\n```code```\"}]}\nnonce";
        let value = serde_json::json!({"messages":[{"role":"user","content":[{"text":user}]}]});
        write_private(&path, &value).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["messages"][0]["content"][0]["text"], user);
        assert!(write_private(&path, &serde_json::json!({"replacement":true})).is_err());
        assert_eq!(saved, serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap());
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn missing_parent_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_private(&dir.path().join("missing/context.json"), &serde_json::json!({})).is_err());
    }
}
