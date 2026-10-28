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
//! moved: 15 12 <branch>
//! state: <state>
//! ```
//!
//! A `moved:` line names a member `land` moves onto trunk, the pull request
//! it was stacked on, and that pull request's head branch, which the member
//! is put back on if the batch does not land. The lines are optional in
//! every state, and each names a member.
//! `state` is one of `started`, `testing <tip>`, `conflict <pr> trunk`,
//! `conflict <pr> pr <m>`, `landing <tip>`, `landed <tip>`,
//! `refused <tip> trunk-moved <sha>`, `refused <tip> member-gone <pr>`,
//! `refused <tip> member-held <pr>`.
//! A probe's record has no `member:` lines.
//! A `ConflictWith` or `LandRefusal` variant with no form here cannot be
//! recorded, and the effect that would record it is `Permanent`; a forge
//! change that constructs a new variant gives it a form in the same change.
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
    /// The stacked members `land` moves onto trunk, in batch order.
    pub moved: Vec<Moved>,
    pub state: RecordState,
}

/// A stacked member `land` moves onto trunk before the fast-forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub pr: PrNumber,
    /// The pull request it was stacked on.
    pub base: PrNumber,
    /// `base`'s head branch, by short name.
    pub branch: String,
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

/// A batch state the record grammar has no form for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unrecordable {
    pub id: BatchId,
    pub state: RecordState,
}

impl fmt::Display for Unrecordable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "batch {}'s state {:?} has no form in the batch record",
            self.id.0, self.state
        )
    }
}

impl Error for Unrecordable {}

impl Record {
    /// The record text; ends with a newline. A state with no form in the
    /// grammar is an error.
    pub fn render(&self) -> Result<String, Unrecordable> {
        let mut text = format!("{HEADER}\nid: {}\nbase: {}\n", self.id.0, self.base);
        for member in &self.members {
            text.push_str(&format!("member: {} {}\n", member.pr.0, member.head));
        }
        for moved in &self.moved {
            text.push_str(&format!(
                "moved: {} {} {}\n",
                moved.pr.0, moved.base.0, moved.branch
            ));
        }
        let state = render_state(&self.state).ok_or_else(|| Unrecordable {
            id: self.id,
            state: self.state.clone(),
        })?;
        text.push_str(&format!("state: {state}\n"));
        Ok(text)
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

        let mut members: Vec<BatchMember> = Vec::new();
        let mut moved = Vec::new();
        let state = loop {
            let line = lines
                .next()
                .ok_or_else(|| error("missing field \"state\"".to_owned()))?;
            if let Some(member) = line.strip_prefix("member: ")
                && moved.is_empty()
            {
                let (pr, head) = member
                    .split_once(' ')
                    .ok_or_else(|| error(format!("bad member line {line:?}")))?;
                members.push(BatchMember::new(pr_number(pr, &error)?, sha(head, &error)?));
            } else if let Some(entry) = line.strip_prefix("moved: ") {
                let [pr, base, name] = entry.split(' ').collect::<Vec<_>>()[..] else {
                    return Err(error(format!("bad moved line {line:?}")));
                };
                let pr = pr_number(pr, &error)?;
                if !members.iter().any(|m| m.pr == pr) {
                    return Err(error(format!("moved line {line:?} names no member")));
                }
                if name.is_empty() {
                    return Err(error(format!("bad moved line {line:?}")));
                }
                moved.push(Moved {
                    pr,
                    base: pr_number(base, &error)?,
                    branch: name.to_owned(),
                });
            } else if let Some(state) = line.strip_prefix("state: ") {
                break parse_state(state, &error)?;
            } else {
                return Err(error(format!("unexpected line {line:?}")));
            }
        };
        if let Some(line) = lines.next() {
            return Err(error(format!("unexpected line {line:?} after state")));
        }

        Ok(Record {
            id,
            base,
            members,
            moved,
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

fn render_state(state: &RecordState) -> Option<String> {
    let text = match state {
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
        RecordState::Refused {
            tip,
            reason: LandRefusal::MemberHeld { pr },
        } => format!("refused {tip} member-held {}", pr.0),
        RecordState::Conflict { .. } | RecordState::Refused { .. } => return None,
    };
    Some(text)
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
        ["refused", tip, "member-held", pr] => RecordState::Refused {
            tip: sha(tip, error)?,
            reason: LandRefusal::MemberHeld {
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
                BatchMember::new(PrNumber(12), sha(B)),
                BatchMember::new(PrNumber(15), sha(C)),
            ],
            moved: Vec::new(),
            state,
        }
    }

    fn stacked(state: RecordState) -> Record {
        Record {
            moved: vec![Moved {
                pr: PrNumber(15),
                base: PrNumber(12),
                branch: "feature/base".to_owned(),
            }],
            ..record(state)
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
            RecordState::Refused {
                tip: sha(T),
                reason: LandRefusal::MemberHeld { pr: PrNumber(15) },
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
        record(RecordState::Testing { tip: sha(T) })
            .render()
            .unwrap()
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
            let text = original.render().unwrap();
            assert_eq!(Record::parse(REF, BatchId(7), &text).unwrap(), original);
            let trimmed = text.trim_end_matches('\n');
            assert_eq!(Record::parse(REF, BatchId(7), trimmed).unwrap(), original);
        }
    }

    #[test]
    fn moved_lines_render_and_round_trip_in_every_state() {
        for state in every_state() {
            let original = stacked(state);
            let text = original.render().unwrap();
            assert!(
                text.contains(&format!(
                    "member: 15 {C}\nmoved: 15 12 feature/base\nstate: "
                )),
                "{text}"
            );
            assert_eq!(Record::parse(REF, BatchId(7), &text).unwrap(), original);
        }
    }

    #[test]
    fn a_landing_record_without_moved_lines_parses() {
        let text = format!(
            "{HEADER}\nid: 7\nbase: {A}\nmember: 12 {B}\nmember: 15 {C}\nstate: landing {T}\n"
        );
        let landing = Record::parse(REF, BatchId(7), &text).unwrap();
        assert!(landing.moved.is_empty());
        assert_eq!(landing, record(RecordState::Landing { tip: sha(T) }));
    }

    #[test]
    fn rejects_malformed_moved_lines() {
        let good = stacked(RecordState::Landing { tip: sha(T) })
            .render()
            .unwrap();
        for (text, problem) in [
            (good.replace("moved: 15 ", "moved: 13 "), "names no member"),
            (
                good.replace("moved: 15 12 ", "moved: 15 x "),
                "bad pull request number",
            ),
            (good.replace(" feature/base", ""), "bad moved line"),
            (good.replace(" feature/base", " "), "bad moved line"),
            (good.replace(" feature/base", " a b"), "bad moved line"),
            (
                good.replace(
                    "moved: 15 12 feature/base\n",
                    &format!("moved: 15 12 feature/base\nmember: 16 {C}\n"),
                ),
                "unexpected line",
            ),
        ] {
            let error = rejected(&text);
            assert!(error.problem.contains(problem), "{text}: {error}");
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
    }

    #[test]
    fn a_probe_record_with_no_members_round_trips() {
        let text = format!("{HEADER}\nid: 7\nbase: {A}\nstate: testing {T}\n");
        let probe = Record::parse(REF, BatchId(7), &text).unwrap();
        assert!(probe.members.is_empty());
        assert_eq!(probe.state, RecordState::Testing { tip: sha(T) });
        assert_eq!(probe.render().unwrap(), text);
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
