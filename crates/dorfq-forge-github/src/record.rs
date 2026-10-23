//! The batch record: the text of the commit `refs/dorfq/batches/<id>`
//! points at, holding the batch's id, base, members and the forge's own
//! verdicts.
//!
//! ```text
//! dorfq batch record v1
//! id: 7
//! base: <sha>
//! member: 12 <sha>
//! member: 15 <sha>
//! state: <state>
//! ```
//!
//! `state` is one of `started`, `testing <tip>`, `conflict <pr> trunk`,
//! `conflict <pr> pr <m>`, `landing <tip>`, `landed <tip>`,
//! `refused <tip> trunk-moved <sha>`, `refused <tip> member-gone <pr>`.
//! Parsing is strict: the forge does not guess about a record it did not
//! write.

use std::error::Error;
use std::fmt;

use dorfq_core::{BatchId, BatchMember, ConflictWith, LandRefusal, PrNumber, Sha};

const HEADER: &str = "dorfq batch record v1";
const HEADER_PREFIX: &str = "dorfq batch record ";

/// One batch, as its record states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: BatchId,
    pub base: Sha,
    /// In batch order.
    pub members: Vec<BatchMember>,
    pub state: RecordState,
}

/// What the forge has decided about a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordState {
    /// Building; no published batch branch is to be trusted.
    Started,
    /// Published at `tip`.
    Testing { tip: Sha },
    /// Git refused to merge `pr` while building.
    Conflict { pr: PrNumber, with: ConflictWith },
    /// `land` has begun; whether trunk moved is read from git.
    Landing { tip: Sha },
    /// Trunk was fast-forwarded to `tip`.
    Landed { tip: Sha },
    /// `land` declined.
    Refused { tip: Sha, reason: LandRefusal },
}

/// A record that does not parse, naming the ref it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordError {
    pub ref_name: String,
    pub problem: String,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "batch record {}: {}", self.ref_name, self.problem)
    }
}

impl Error for RecordError {}

impl Record {
    /// The record text; ends with a newline.
    pub fn render(&self) -> String {
        let mut text = format!("{HEADER}\nid: {}\nbase: {}\n", self.id.0, self.base);
        for member in &self.members {
            text.push_str(&format!("member: {} {}\n", member.pr.0, member.head));
        }
        text.push_str(&format!("state: {}\n", render_state(&self.state)));
        text
    }

    /// Parses the record read from `ref_name`, whose suffix says it is
    /// batch `id`.
    pub fn parse(ref_name: &str, id: BatchId, text: &str) -> Result<Record, RecordError> {
        let error = |problem: String| RecordError {
            ref_name: ref_name.to_owned(),
            problem,
        };
        let body = text.strip_suffix('\n').unwrap_or(text);
        let mut lines = body.split('\n');

        match lines.next() {
            Some(HEADER) => {}
            Some(line) if line.starts_with(HEADER_PREFIX) => {
                return Err(error(format!("unsupported version in {line:?}")));
            }
            _ => return Err(error("not a dorfq batch record".to_owned())),
        }

        let recorded =
            BatchId(field(&mut lines, "id", &error).and_then(|v| {
                parse_decimal(v).ok_or_else(|| error(format!("bad batch id {v:?}")))
            })?);
        if recorded != id {
            return Err(error(format!(
                "records batch {} under the ref of batch {}",
                recorded.0, id.0
            )));
        }
        let base = field(&mut lines, "base", &error).and_then(|v| sha(v, &error))?;

        let mut members = Vec::new();
        let state = loop {
            let line = lines
                .next()
                .ok_or_else(|| error("missing field \"state\"".to_owned()))?;
            if let Some(member) = line.strip_prefix("member: ") {
                let (pr, head) = member
                    .split_once(' ')
                    .ok_or_else(|| error(format!("bad member line {line:?}")))?;
                members.push(BatchMember {
                    pr: pr_number(pr, &error)?,
                    head: sha(head, &error)?,
                });
            } else if let Some(state) = line.strip_prefix("state: ") {
                break parse_state(state, &error)?;
            } else {
                return Err(error(format!("unexpected line {line:?}")));
            }
        };
        if members.is_empty() {
            return Err(error("no members".to_owned()));
        }
        if let Some(line) = lines.next() {
            return Err(error(format!("unexpected line {line:?} after state")));
        }

        Ok(Record {
            id,
            base,
            members,
            state,
        })
    }
}

/// A decimal with no sign and no leading zero, as the forge writes ids and
/// pull request numbers.
pub fn parse_decimal(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if canonical { text.parse().ok() } else { None }
}

fn render_state(state: &RecordState) -> String {
    match state {
        RecordState::Started => "started".to_owned(),
        RecordState::Testing { tip } => format!("testing {tip}"),
        RecordState::Conflict {
            pr,
            with: ConflictWith::Trunk,
        } => format!("conflict {} trunk", pr.0),
        RecordState::Conflict {
            pr,
            with: ConflictWith::Pr(other),
        } => format!("conflict {} pr {}", pr.0, other.0),
        RecordState::Landing { tip } => format!("landing {tip}"),
        RecordState::Landed { tip } => format!("landed {tip}"),
        RecordState::Refused {
            tip,
            reason: LandRefusal::TrunkMoved { trunk },
        } => format!("refused {tip} trunk-moved {trunk}"),
        RecordState::Refused {
            tip,
            reason: LandRefusal::MemberGone { pr },
        } => format!("refused {tip} member-gone {}", pr.0),
    }
}

fn parse_state(
    text: &str,
    error: &impl Fn(String) -> RecordError,
) -> Result<RecordState, RecordError> {
    let words: Vec<&str> = text.split(' ').collect();
    let state = match words.as_slice() {
        ["started"] => RecordState::Started,
        ["testing", tip] => RecordState::Testing {
            tip: sha(tip, error)?,
        },
        ["conflict", pr, "trunk"] => RecordState::Conflict {
            pr: pr_number(pr, error)?,
            with: ConflictWith::Trunk,
        },
        ["conflict", pr, "pr", other] => RecordState::Conflict {
            pr: pr_number(pr, error)?,
            with: ConflictWith::Pr(pr_number(other, error)?),
        },
        ["landing", tip] => RecordState::Landing {
            tip: sha(tip, error)?,
        },
        ["landed", tip] => RecordState::Landed {
            tip: sha(tip, error)?,
        },
        ["refused", tip, "trunk-moved", trunk] => RecordState::Refused {
            tip: sha(tip, error)?,
            reason: LandRefusal::TrunkMoved {
                trunk: sha(trunk, error)?,
            },
        },
        ["refused", tip, "member-gone", pr] => RecordState::Refused {
            tip: sha(tip, error)?,
            reason: LandRefusal::MemberGone {
                pr: pr_number(pr, error)?,
            },
        },
        _ => return Err(error(format!("unknown state {text:?}"))),
    };
    Ok(state)
}

fn field<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    name: &str,
    error: &impl Fn(String) -> RecordError,
) -> Result<&'a str, RecordError> {
    let line = lines
        .next()
        .ok_or_else(|| error(format!("missing field {name:?}")))?;
    line.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(": "))
        .ok_or_else(|| error(format!("expected field {name:?}, found {line:?}")))
}

fn sha(text: &str, error: &impl Fn(String) -> RecordError) -> Result<Sha, RecordError> {
    Sha::parse(text).map_err(|e| error(e.to_string()))
}

fn pr_number(text: &str, error: &impl Fn(String) -> RecordError) -> Result<PrNumber, RecordError> {
    parse_decimal(text)
        .map(PrNumber)
        .ok_or_else(|| error(format!("bad pull request number {text:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const T: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const REF: &str = "refs/dorfq/batches/7";

    fn sha(s: &str) -> Sha {
        Sha::parse(s).unwrap()
    }

    fn record(state: RecordState) -> Record {
        Record {
            id: BatchId(7),
            base: sha(A),
            members: vec![
                BatchMember {
                    pr: PrNumber(12),
                    head: sha(B),
                },
                BatchMember {
                    pr: PrNumber(15),
                    head: sha(C),
                },
            ],
            state,
        }
    }

    fn every_state() -> Vec<RecordState> {
        vec![
            RecordState::Started,
            RecordState::Testing { tip: sha(T) },
            RecordState::Conflict {
                pr: PrNumber(15),
                with: ConflictWith::Trunk,
            },
            RecordState::Conflict {
                pr: PrNumber(15),
                with: ConflictWith::Pr(PrNumber(12)),
            },
            RecordState::Landing { tip: sha(T) },
            RecordState::Landed { tip: sha(T) },
            RecordState::Refused {
                tip: sha(T),
                reason: LandRefusal::TrunkMoved { trunk: sha(C) },
            },
            RecordState::Refused {
                tip: sha(T),
                reason: LandRefusal::MemberGone { pr: PrNumber(12) },
            },
        ]
    }

    fn rejected(text: &str) -> RecordError {
        let error = Record::parse(REF, BatchId(7), text).unwrap_err();
        assert_eq!(error.ref_name, REF);
        assert!(error.to_string().contains(REF), "{error}");
        error
    }

    fn valid_text() -> String {
        record(RecordState::Testing { tip: sha(T) }).render()
    }

    #[test]
    fn renders_the_documented_text() {
        assert_eq!(
            valid_text(),
            format!(
                "dorfq batch record v1\nid: 7\nbase: {A}\nmember: 12 {B}\nmember: 15 {C}\nstate: testing {T}\n"
            )
        );
    }

    #[test]
    fn every_state_round_trips() {
        for state in every_state() {
            let original = record(state);
            let text = original.render();
            assert_eq!(Record::parse(REF, BatchId(7), &text).unwrap(), original);
            let trimmed = text.trim_end_matches('\n');
            assert_eq!(Record::parse(REF, BatchId(7), trimmed).unwrap(), original);
        }
    }

    #[test]
    fn rejects_other_versions_and_foreign_text() {
        let v2 = valid_text().replace("v1", "v2");
        assert!(rejected(&v2).problem.contains("version"));
        rejected("Merge pull request #3");
        rejected("");
    }

    #[test]
    fn rejects_a_mismatched_id() {
        let error = Record::parse(REF, BatchId(8), &valid_text()).unwrap_err();
        assert!(error.problem.contains("batch 7"), "{error}");
    }

    #[test]
    fn rejects_missing_fields() {
        let text = valid_text();
        let lines: Vec<&str> = text.lines().collect();
        for skip in 1..lines.len() {
            let without: Vec<&str> = lines
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, l)| *l)
                .collect();
            if lines[skip].starts_with("member: ") {
                continue;
            }
            rejected(&without.join("\n"));
        }
        let no_members = format!("{HEADER}\nid: 7\nbase: {A}\nstate: started\n");
        assert_eq!(rejected(&no_members).problem, "no members");
    }

    #[test]
    fn rejects_malformed_lines() {
        let good = valid_text();
        let cases = [
            good.replace("id: 7", "id: 07"),
            good.replace("id: 7", "id: +7"),
            good.replace("id: 7", "id:7"),
            good.replace("member: 12 ", "member: 12  "),
            good.replace("member: 12 ", "member: x "),
            good.replace("member: 12 ", "member 12 "),
            good.replace("member: 15 ", "member: 015 "),
            format!("{good}extra: 1\n"),
            good.replace("state: testing", "state: testing  "),
            good.replace("\nbase:", "\n\nbase:"),
            good.replace("\nstate:", "\ncomment: hi\nstate:"),
        ];
        for text in cases {
            rejected(&text);
        }
    }

    #[test]
    fn rejects_a_bad_sha() {
        let upper = valid_text().replace(
            &format!("base: {A}"),
            &format!("base: {}", A.to_uppercase()),
        );
        assert!(rejected(&upper).problem.contains("invalid commit id"));
        let short = valid_text().replace(&format!("testing {T}"), "testing abc");
        rejected(&short);
    }

    #[test]
    fn rejects_unknown_states() {
        let good = valid_text();
        let testing = format!("testing {T}");
        for (state, problem) in [
            ("building".to_owned(), "unknown state"),
            ("testing".to_owned(), "unknown state"),
            (format!("landed {T} extra"), "unknown state"),
            ("conflict 15 main".to_owned(), "unknown state"),
            ("conflict 15 pr".to_owned(), "unknown state"),
            (format!("refused {T} trunk-moved"), "unknown state"),
            (
                format!("refused {T} member-gone x"),
                "bad pull request number",
            ),
            (format!("refused {T} gone 12"), "unknown state"),
        ] {
            let error = rejected(&good.replace(&testing, &state));
            assert!(error.problem.contains(problem), "{state}: {error}");
        }
    }

    #[test]
    fn decimal_is_canonical() {
        assert_eq!(parse_decimal("0"), Some(0));
        assert_eq!(parse_decimal("42"), Some(42));
        assert_eq!(parse_decimal(""), None);
        assert_eq!(parse_decimal("042"), None);
        assert_eq!(parse_decimal("-1"), None);
        assert_eq!(parse_decimal("1a"), None);
        assert_eq!(parse_decimal("99999999999999999999"), None);
    }
}
