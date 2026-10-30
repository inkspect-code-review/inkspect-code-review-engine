//! Repository review exclusions: the root `.inkspectignore` file.
//!
//! A repository lists paths Inkspect should leave out of reviews, one pattern
//! per line. The policy is read from the trusted comparison BASE revision (the
//! merge base), never from the reviewed head, so a pull request cannot exclude
//! its own changes; an edited policy is reviewed like any other change and
//! applies once merged. The same rules filter the review diff, the retrieval
//! candidates used as context, and the repository index.
//!
//! Grammar (compatible with existing `.inkspectignore` files):
//!   * Surrounding whitespace is trimmed. Blank lines and lines starting with
//!     `#` are ignored; there are no trailing comments.
//!   * `*` matches any run of characters within one path segment, `?` exactly
//!     one character, and `**` as a whole segment any number of segments.
//!   * A leading `/`, or a `/` between segments, anchors the pattern to the
//!     repository root. A single segment otherwise matches at any depth.
//!   * A trailing `/` matches only paths below a matching directory.
//!   * `!` re-includes. Every rule is evaluated and the last match wins, so a
//!     child of an excluded directory can be re-included.
//!   * A leading `\#` or `\!` matches a literal `#` or `!`.
//!
//! Character classes, other backslash escapes and nested ignore files are not
//! supported. Matching is case-sensitive. Anything invalid or over the limits
//! fails the run with `source.invalid_ignore` before any model call: guessing
//! which files a team meant to exclude could send them for review anyway.
//!
//! This scopes reviews. It is not access control or a secret scanner.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{err, Code, EngineError, Result};

/// The policy file name. Only the copy at the repository root is read.
pub const IGNORE_FILE: &str = ".inkspectignore";

const MAX_BYTES: usize = 32 * 1024;
const MAX_LINES: usize = 1025;
const MAX_RULE_BYTES: usize = 512;
const MAX_RULES: usize = 512;

#[test]
fn starter_policy_excludes_generated_and_private_files_but_keeps_review_inputs() {
    let policy = IgnorePolicy::parse(include_bytes!("../.inkspectignore")).unwrap();
    for path in ["target/debug/engine", "web/node_modules/pkg/index.js", ".env.local",
                 "config/signing.pem", "secrets/token.txt", "public/app.min.js"] {
        assert!(policy.excluded(path), "expected exclusion: {path}");
    }
    for path in [".env.example", "Cargo.lock", ".github/workflows/ci.yml",
                 "tests/review.rs", "src/main.rs", ".inkspectignore"] {
        assert!(!policy.excluded(path), "expected review input: {path}");
    }
}

/// A parsed exclusion policy, pinned to the revision it was read from.
#[derive(Debug, Clone)]
pub struct IgnorePolicy {
    rules: Vec<Rule>,
    /// `sha256:<hex>` over the exact file bytes.
    pub digest: String,
}

#[derive(Debug, Clone)]
struct Rule {
    /// A `!` rule re-includes paths excluded by earlier rules.
    include: bool,
    /// Matches from the repository root only.
    rooted: bool,
    /// Trailing `/`: matches only paths below a matching directory.
    directory: bool,
    segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
enum Segment {
    /// `**`: any number of whole path segments.
    AnyDepth,
    /// A glob over exactly one path segment.
    Glob(Vec<Token>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Literal(char),
    /// `?`: exactly one character.
    One,
    /// `*`: any run of characters, possibly empty.
    Any,
}

impl IgnorePolicy {
    /// Parse the policy file bytes. Invalid content is `source.invalid_ignore`.
    pub fn parse(content: &[u8]) -> Result<Self> {
        if content.len() > MAX_BYTES {
            return Err(invalid("the file exceeds 32 KiB"));
        }
        let text = std::str::from_utf8(content).map_err(|_| invalid("the file is not UTF-8"))?;
        if text.contains('\0') {
            return Err(invalid("the file contains a NUL byte"));
        }
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        // A final newline ends the last line rather than starting another.
        let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
        if lines.len() > MAX_LINES {
            return Err(invalid("the file exceeds 1025 lines"));
        }
        let mut rules = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.len() > MAX_RULE_BYTES || rules.len() >= MAX_RULES {
                return Err(invalid_at(index, "exceeds the rule limits"));
            }
            rules.push(Rule::parse(line).map_err(|problem| invalid_at(index, problem))?);
        }
        Ok(Self {
            rules,
            digest: format!("sha256:{}", hex::encode(Sha256::digest(content))),
        })
    }

    /// Whether the repository-relative, `/`-separated `path` is excluded. The
    /// root policy file itself always stays reviewable.
    pub fn excluded(&self, path: &str) -> bool {
        if path == IGNORE_FILE {
            return false;
        }
        let segments: Vec<Vec<char>> = path.split('/').map(|s| s.chars().collect()).collect();
        // The last matching rule wins, so the first match from the end decides.
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches(&segments))
            .is_some_and(|rule| !rule.include)
    }
}

/// Read the root `.inkspectignore` at `revision` from the prepared clone.
/// Returns `Ok(None)` when the revision has no policy file.
pub async fn load(repo_dir: &Path, revision: &str, token: &str) -> Result<Option<IgnorePolicy>> {
    // Trees are always present in the partial clone, so listing needs no
    // network. An absent path lists nothing and exits successfully.
    let listing = crate::git::index_git(repo_dir, &["ls-tree", "-z", revision, "--", IGNORE_FILE], token).await?;
    if listing.is_empty() {
        return Ok(None);
    }
    // "<mode> <type> <object>\t<path>\0"
    let listing = String::from_utf8_lossy(&listing);
    let unreadable = || err(Code::Source, "unreadable .inkspectignore entry");
    let (meta, _) = listing.split_once('\t').ok_or_else(unreadable)?;
    let mut fields = meta.split(' ');
    let (mode, kind, object) = (fields.next(), fields.next(), fields.next().ok_or_else(unreadable)?);
    if kind != Some("blob") || !matches!(mode, Some("100644" | "100755")) {
        return Err(invalid("the file must be a regular file"));
    }
    if object.is_empty() || !object.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unreadable());
    }
    // Check the size before reading the blob into memory.
    let size = crate::git::index_git(repo_dir, &["cat-file", "-s", object], token).await?;
    let size: usize = String::from_utf8_lossy(&size).trim().parse().map_err(|_| unreadable())?;
    if size > MAX_BYTES {
        return Err(invalid("the file exceeds 32 KiB"));
    }
    let content = crate::git::index_git(repo_dir, &["cat-file", "blob", object], token).await?;
    IgnorePolicy::parse(&content).map(Some)
}

/// Reasons name the file and, at most, a line number; never its contents.
fn invalid(problem: &str) -> EngineError {
    err(Code::IgnoreInvalid, format!("{IGNORE_FILE}: {problem}"))
}

fn invalid_at(index: usize, problem: &str) -> EngineError {
    err(Code::IgnoreInvalid, format!("{IGNORE_FILE} line {}: {problem}", index + 1))
}

impl Rule {
    fn parse(line: &str) -> std::result::Result<Self, &'static str> {
        let (include, pattern) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        // `\#` and `\!` escape a leading literal; no other escapes exist.
        let pattern = if pattern.starts_with("\\#") || pattern.starts_with("\\!") {
            &pattern[1..]
        } else {
            pattern
        };
        if pattern.contains(['\\', '[', ']', '\r', '\t']) {
            return Err("uses an unsupported character");
        }
        let anchored = pattern.starts_with('/');
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        let directory = pattern.ends_with('/');
        let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
        if pattern.is_empty() {
            return Err("has an empty pattern");
        }
        let parts: Vec<&str> = pattern.split('/').collect();
        let mut segments = Vec::with_capacity(parts.len());
        for part in &parts {
            if part.is_empty() || *part == "." || *part == ".." {
                return Err("has an empty, . or .. segment");
            }
            if *part == "**" {
                segments.push(Segment::AnyDepth);
            } else if part.contains("**") {
                return Err("uses ** inside a segment");
            } else {
                segments.push(Segment::Glob(
                    part.chars()
                        .map(|c| match c {
                            '*' => Token::Any,
                            '?' => Token::One,
                            c => Token::Literal(c),
                        })
                        .collect(),
                ));
            }
        }
        Ok(Self {
            include,
            rooted: anchored || parts.len() > 1,
            directory,
            segments,
        })
    }

    /// Run the pattern over the path segments. `reach[p]` means the pattern
    /// segments seen so far can end right before path segment `p`. Linear in
    /// pattern segments times path depth, so hostile patterns cannot blow up.
    fn matches(&self, path: &[Vec<char>]) -> bool {
        let n = path.len();
        let mut reach = vec![false; n + 1];
        if self.rooted {
            reach[0] = true;
        } else {
            reach[..n].fill(true);
        }
        let mut next = vec![false; n + 1];
        for (index, segment) in self.segments.iter().enumerate() {
            next.fill(false);
            match segment {
                Segment::AnyDepth => {
                    // A trailing `**` consumes at least one segment (`a/**`
                    // never matches `a` itself); elsewhere it may consume none.
                    let trailing = index + 1 == self.segments.len();
                    let mut seen = false;
                    for p in 0..=n {
                        if !trailing {
                            seen |= reach[p];
                        }
                        next[p] = seen;
                        if trailing {
                            seen |= reach[p];
                        }
                    }
                }
                Segment::Glob(tokens) => {
                    for p in 0..n {
                        if reach[p] && glob(tokens, &path[p]) {
                            next[p + 1] = true;
                        }
                    }
                }
            }
            std::mem::swap(&mut reach, &mut next);
            if !reach.contains(&true) {
                return false;
            }
        }
        // A directory rule needs at least one path segment below the match;
        // otherwise the match may end at the path or at any parent of it.
        let ends = if self.directory { &reach[..n] } else { &reach[..] };
        ends.contains(&true)
    }
}

/// Wildcard match of one path segment: `*` any run, `?` one character.
fn glob(tokens: &[Token], text: &[char]) -> bool {
    let (mut t, mut s) = (0, 0);
    // Token index after the most recent `*`, and the text index it resumes from.
    let mut star: Option<(usize, usize)> = None;
    while s < text.len() {
        match tokens.get(t) {
            Some(Token::Any) => {
                star = Some((t + 1, s));
                t += 1;
            }
            Some(Token::One) => {
                t += 1;
                s += 1;
            }
            Some(Token::Literal(c)) if *c == text[s] => {
                t += 1;
                s += 1;
            }
            _ => match star {
                // Let the last `*` absorb one more character, then retry.
                Some((after, from)) => {
                    star = Some((after, from + 1));
                    t = after;
                    s = from + 1;
                }
                None => return false,
            },
        }
    }
    tokens[t..].iter().all(|token| *token == Token::Any)
}
