//! Bounded review context for large supporting files. Only byte-identical runs
//! are omitted; every changed line and nearby context retains its source number.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn excerpts(before: &str, after: &str) -> Value {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let (mut i, mut j) = (0, 0);
    let mut spans = Vec::new();
    while i < old.len() || j < new.len() {
        let (a, b) = (i, j);
        while i < old.len() && j < new.len() && old[i] == new[j] { i += 1; j += 1; }
        if i > a {
            let count = i-a;
            if count > 8 {
                spans.push(json!({"original_start_line":a+1,"proposed_start_line":b+1,"original":old[a..a+3].concat(),"proposed":new[b..b+3].concat()}));
                spans.push(json!({"original_start_line":a+4,"proposed_start_line":b+4,"omitted_identical_lines":count-6}));
                spans.push(json!({"original_start_line":i-2,"proposed_start_line":j-2,"original":old[i-3..i].concat(),"proposed":new[j-3..j].concat()}));
            } else {
                spans.push(json!({"original_start_line":a+1,"proposed_start_line":b+1,"original":old[a..i].concat(),"proposed":new[b..j].concat()}));
            }
            continue;
        }
        // Find a nearby synchronization line. A nonminimal alignment only adds
        // context; it can never hide a change. The caller still enforces budget.
        let mut next = None;
        'search: for distance in 1..=256 {
            for left in 0..=distance {
                let right = distance-left;
                if i+left < old.len() && j+right < new.len() && old[i+left] == new[j+right] {
                    next = Some((i+left,j+right)); break 'search;
                }
            }
        }
        let (end_old,end_new) = next.unwrap_or((old.len(),new.len()));
        spans.push(json!({"original_start_line":i+1,"proposed_start_line":j+1,"original":old[i..end_old].concat(),"proposed":new[j..end_new].concat()}));
        i=end_old;j=end_new;
    }
    json!({"format":"exact_changed_lines_with_context","original_sha256":hex::encode(Sha256::digest(before)),
        "proposed_sha256":hex::encode(Sha256::digest(after)),"spans":spans})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assert_complete(before: &str, after: &str) -> Value {
        let value=excerpts(before,after);
        let old:Vec<_>=before.split_inclusive('\n').collect();
        let new:Vec<_>=after.split_inclusive('\n').collect();
        let (mut a,mut b)=(0,0);
        for span in value["spans"].as_array().unwrap() {
            assert_eq!(span["original_start_line"].as_u64().unwrap() as usize,a+1);
            assert_eq!(span["proposed_start_line"].as_u64().unwrap() as usize,b+1);
            if let Some(n)=span["omitted_identical_lines"].as_u64() {
                let n=n as usize;assert_eq!(old[a..a+n],new[b..b+n]);a+=n;b+=n;
            } else {
                for (text,lines,pos) in [("original",&old,&mut a),("proposed",&new,&mut b)] {
                    let text=span[text].as_str().unwrap();let n=text.split_inclusive('\n').count();
                    assert_eq!(text,lines[*pos..*pos+n].concat());*pos+=n;
                }
            }
        }
        assert_eq!((a,b),(old.len(),new.len()));value
    }
    #[test]
    fn retains_every_changed_byte_and_exact_line_numbers() {
        let before=(0..15000).map(|n|format!("  \"package-{n}\": {{\"version\":\"1\"}},\n")).collect::<String>();
        let after=before.replace("\"package-30\": {\"version\":\"1\"}","\"package-30\": {\"version\":\"2\"}")
            .replace("\"package-12000\"","\"new-package\"");
        let context=assert_complete(&before,&after).to_string();
        assert!(context.len()<10000);assert!(context.contains("new-package"));
        assert_ne!(excerpts(&before,&after)["original_sha256"],excerpts(&before,&after)["proposed_sha256"]);
        for (a,b) in [("","added\n"),("removed\n",""),("a\nb\n","a\nx\nb\n"),("x\nx\na\n","x\na\n"),("a\n","a"),("a\n","b\n")] {assert_complete(a,b);}
    }
}
