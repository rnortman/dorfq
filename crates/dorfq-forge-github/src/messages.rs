//! What the forge tells authors (rule 9): one issue comment per transition,
//! each beginning with a hidden marker so the comment thread is the forge's
//! own journal of what it has said.
//!
//! The marker is `<!-- dorfq: <event> -->`, with `event` one of `queued`,
//! `held`, `testing`, `landed`, `ejected`, `notice`, `withdrawn`,
//! `blocked`; a `blocked` marker also names the blocker, `blocked #<n>` or
//! `blocked branch <name>`. A `notice` is the scheduler telling the author
//! what it believes; it is not a transition, the pull request stays queued,
//! and every reading of the thread skips it. A pull request whose last
//! transition marker is `queued`, `held` or `testing` was greeted already;
//! any other, or none, means it is entering the queue.

use std::collections::BTreeSet;
use std::fmt;

use dorfq_core::{BatchId, ConflictWith, EjectReason, Hold, NoticeReason, PrBase, PrNumber, Sha};

/// Something the forge tells a pull request's author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub pr: PrNumber,
    pub event: Event,
}

/// The transitions authors hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The pull request entered the queue.
    Queued,
    /// The pull request is enqueued but held back for `holds`.
    Held { holds: BTreeSet<Hold> },
    /// The pull request is in batch `batch`, published at `tip`.
    Testing { batch: BatchId, tip: Sha },
    /// The pull request landed with batch `batch`; trunk was fast-forwarded
    /// to `tip`.
    Landed { batch: BatchId, tip: Sha },
    /// The scheduler ejected the pull request, which was based on `base`.
    Ejected { reason: EjectReason, base: PrBase },
    /// The scheduler told the author what it believes; the pull request
    /// stays queued.
    Notice { reason: NoticeReason },
    /// The pull request left the queue by neither landing nor ejection.
    Withdrawn,
    /// The pull request carries the label but is not enqueued, for `on`.
    Blocked { on: Blocker },
}

/// Why a labelled pull request is not enqueued.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Blocker {
    /// Its base is the head branch of `pr`, which is not enqueued.
    Pr(PrNumber),
    /// Its base is a branch that is neither trunk nor an enqueued pull
    /// request's head.
    Branch(String),
}

/// `#<n>` or `branch <name>`, as the sink line and the marker name it.
impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::Pr(pr) => write!(f, "#{}", pr.0),
            Blocker::Branch(name) => write!(f, "branch {name}"),
        }
    }
}

/// The kind of event a comment's marker names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Queued,
    Held,
    Testing,
    Landed,
    Ejected,
    Notice,
    Withdrawn,
    Blocked,
}

const MARKER_OPEN: &str = "<!-- dorfq: ";
const MARKER_CLOSE: &str = " -->";

impl Marker {
    fn word(self) -> &'static str {
        match self {
            Marker::Queued => "queued",
            Marker::Held => "held",
            Marker::Testing => "testing",
            Marker::Landed => "landed",
            Marker::Ejected => "ejected",
            Marker::Notice => "notice",
            Marker::Withdrawn => "withdrawn",
            Marker::Blocked => "blocked",
        }
    }

    /// The marker a comment body begins with, if any; what follows the
    /// event word is not read.
    pub fn of(body: &str) -> Option<Marker> {
        let rest = body.strip_prefix(MARKER_OPEN)?;
        let (text, _) = rest.split_once(MARKER_CLOSE)?;
        let word = text.split_once(' ').map_or(text, |(word, _)| word);
        [
            Marker::Queued,
            Marker::Held,
            Marker::Testing,
            Marker::Landed,
            Marker::Ejected,
            Marker::Notice,
            Marker::Withdrawn,
            Marker::Blocked,
        ]
        .into_iter()
        .find(|m| m.word() == word)
    }
}

/// The marker of the last comment in `bodies` (oldest first) that carries
/// one.
pub fn last_marker(bodies: &[String]) -> Option<Marker> {
    bodies.iter().rev().find_map(|body| Marker::of(body))
}

/// The last comment in `bodies` (oldest first) whose marker is a
/// transition, with that marker: a `notice` is skipped.
fn last_transition(bodies: &[String]) -> Option<(&str, Marker)> {
    bodies
        .iter()
        .rev()
        .find_map(|body| match Marker::of(body)? {
            Marker::Notice => None,
            marker => Some((body.as_str(), marker)),
        })
}

/// The marker of the last comment in `bodies` (oldest first) that carries
/// a transition marker.
pub fn last_transition_marker(bodies: &[String]) -> Option<Marker> {
    last_transition(bodies).map(|(_, marker)| marker)
}

/// Whether the last comment in `bodies` (oldest first) that carries a
/// transition marker told the author the pull request is blocked on `on`.
pub fn told_blocked(bodies: &[String], on: &Blocker) -> bool {
    let marker = format!("{MARKER_OPEN}{}{MARKER_CLOSE}", blocked_marker(on));
    last_transition(bodies).is_some_and(|(body, _)| body.starts_with(&marker))
}

/// The text of the marker of a comment saying blocked on `on`.
fn blocked_marker(on: &Blocker) -> String {
    format!("{} {on}", Marker::Blocked.word())
}

/// Whether a pull request with this comment thread, seen entering the queue
/// by a process that did not know it, is to be greeted.
pub fn needs_greeting(bodies: &[String]) -> bool {
    !matches!(
        last_transition_marker(bodies),
        Some(Marker::Queued | Marker::Held | Marker::Testing)
    )
}

impl Event {
    pub fn marker(&self) -> Marker {
        match self {
            Event::Queued => Marker::Queued,
            Event::Held { .. } => Marker::Held,
            Event::Testing { .. } => Marker::Testing,
            Event::Landed { .. } => Marker::Landed,
            Event::Ejected { .. } => Marker::Ejected,
            Event::Notice { .. } => Marker::Notice,
            Event::Withdrawn => Marker::Withdrawn,
            Event::Blocked { .. } => Marker::Blocked,
        }
    }
}

impl Message {
    /// The comment body: the marker line, then the text, in Markdown.
    /// `trunk` and `label` are the configured branch and intent label.
    pub fn body(&self, trunk: &str, label: &str) -> String {
        let text = match &self.event {
            Event::Queued => format!(
                "This pull request is in the merge queue. It will be tested in a batch \
                 with other queued pull requests and landed by fast-forwarding `{trunk}` \
                 when the batch passes. Removing the `{label}` label withdraws it."
            ),
            Event::Held { holds } => format!(
                "Waiting in the merge queue: {}. It joins a batch once nothing holds it.",
                list(holds)
            ),
            Event::Testing { batch, tip } => format!(
                "Testing in batch {batch}, branch `{branch}`, tip {tip}.",
                batch = batch.0,
                branch = crate::batch_branch(*batch)
            ),
            Event::Landed { batch, tip } => {
                format!("Landed in batch {}; `{trunk}` is now {tip}.", batch.0)
            }
            Event::Ejected { reason, base } => {
                format!(
                    "Ejected from the merge queue: {reason}. {}",
                    next_step(reason, *base, trunk, label)
                )
            }
            Event::Notice { reason } => notice(reason, trunk),
            Event::Withdrawn => {
                "Left the merge queue (label removed, pull request closed, or retargeted)."
                    .to_owned()
            }
            Event::Blocked {
                on: Blocker::Pr(pr),
            } => format!(
                "Not in the merge queue: its base #{pr} is not queued. It enters the queue \
                 once #{pr} does, or retarget it onto `{trunk}`.",
                pr = pr.0
            ),
            Event::Blocked {
                on: Blocker::Branch(name),
            } => format!(
                "Not in the merge queue: it targets `{name}`, which is neither `{trunk}` nor \
                 the branch of a queued pull request. Retarget it onto `{trunk}` or onto a \
                 queued pull request's branch."
            ),
        };
        let marker = match &self.event {
            Event::Blocked { on } => blocked_marker(on),
            event => event.marker().word().to_owned(),
        };
        format!("{MARKER_OPEN}{marker}{MARKER_CLOSE}\n{text}\n")
    }
}

/// What an ejected pull request based on `base` does next: a sentence per
/// reason this forge knows, and a generic one for the rest.
fn next_step(reason: &EjectReason, base: PrBase, trunk: &str, label: &str) -> String {
    match reason {
        EjectReason::Conflicts {
            with: ConflictWith::Pr(pr),
        } if base == PrBase::Pr(*pr) => format!(
            "It does not merge into its base #{pr}. Rebase onto #{pr}'s branch, then re-add \
             the `{label}` label.",
            pr = pr.0
        ),
        EjectReason::Conflicts {
            with: ConflictWith::Trunk,
        } => format!("Merge or rebase onto `{trunk}`, then re-add the `{label}` label."),
        EjectReason::Conflicts {
            with: ConflictWith::Pr(pr),
        } => format!(
            "#{pr} is ahead of it in the queue. Once #{pr} lands, rebase onto `{trunk}`, \
             then re-add the `{label}` label.",
            pr = pr.0
        ),
        EjectReason::BatchFailed { batch } => format!(
            "The batch failed its checks on `{branch}`. Re-add the `{label}` label to try \
             again.",
            branch = crate::batch_branch(*batch)
        ),
        _ => format!("Re-add the `{label}` label once that is addressed."),
    }
}

/// What the scheduler believes about a queued pull request, in a sentence
/// per reason.
fn notice(reason: &NoticeReason, trunk: &str) -> String {
    match reason {
        NoticeReason::Broken { batch } => format!(
            "Heads-up from the merge queue: this pull request is probably broken against \
             `{trunk}`. Batch {n} (`{branch}`) failed and nothing else in it explains the \
             failure. It stays in the queue and is ejected only if the queue becomes sure; \
             to settle it sooner, rebase onto `{trunk}` and let its own checks run again.",
            n = batch.0,
            branch = crate::batch_branch(*batch)
        ),
        NoticeReason::InteractsWith { pr, batch } => format!(
            "Heads-up from the merge queue: this pull request probably passes on its own but \
             fails together with #{pr}, last in batch {n} (`{branch}`). It stays in the queue \
             and is not tested with #{pr} again. If #{pr} lands first, this one is tested alone \
             on the new `{trunk}` and ejected if that fails; to settle it sooner, rebase onto \
             `{trunk}` once #{pr} lands and fix the interaction. If this one lands first, the \
             interaction is #{pr}'s to fix.",
            pr = pr.0,
            n = batch.0,
            branch = crate::batch_branch(*batch)
        ),
    }
}

/// The holds, comma-separated, in their order.
fn list(holds: &BTreeSet<Hold>) -> String {
    let words: Vec<String> = holds.iter().map(ToString::to_string).collect();
    words.join(", ")
}

/// The one-line form written to the forge's sink, in the mock forge's
/// format, so a run's trace reads the same whichever forge produced it.
impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message to pr {}: ", self.pr.0)?;
        match &self.event {
            Event::Queued => f.write_str("queued"),
            Event::Held { holds } => write!(f, "held: {}", list(holds)),
            Event::Testing { batch, .. } => write!(f, "testing in batch {}", batch.0),
            Event::Landed { tip, .. } => write!(f, "landed as {tip}"),
            Event::Ejected { reason, .. } => write!(f, "ejected: {reason}"),
            Event::Notice { reason } => write!(f, "notice: {reason}"),
            Event::Withdrawn => f.write_str("withdrawn"),
            Event::Blocked { on } => write!(f, "blocked on {on}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIP: &str = "0123456789abcdef0123456789abcdef01234567";

    fn message(event: Event) -> Message {
        Message {
            pr: PrNumber(12),
            event,
        }
    }

    fn ejected(reason: EjectReason) -> Event {
        Event::Ejected {
            reason,
            base: PrBase::Trunk,
        }
    }

    fn every_event() -> Vec<Event> {
        let tip = Sha::parse(TIP).unwrap();
        vec![
            Event::Queued,
            Event::Held {
                holds: BTreeSet::from([Hold::MergeabilityUnknown, Hold::OwnCiPending]),
            },
            Event::Testing {
                batch: BatchId(7),
                tip: tip.clone(),
            },
            Event::Landed {
                batch: BatchId(7),
                tip,
            },
            ejected(EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            }),
            ejected(EjectReason::Conflicts {
                with: ConflictWith::Pr(PrNumber(9)),
            }),
            ejected(EjectReason::BatchFailed { batch: BatchId(7) }),
            Event::Withdrawn,
            Event::Blocked {
                on: Blocker::Pr(PrNumber(9)),
            },
            Event::Blocked {
                on: Blocker::Branch("release".to_owned()),
            },
            Event::Ejected {
                reason: EjectReason::Conflicts {
                    with: ConflictWith::Pr(PrNumber(9)),
                },
                base: PrBase::Pr(PrNumber(9)),
            },
            Event::Notice {
                reason: NoticeReason::Broken { batch: BatchId(7) },
            },
            Event::Notice {
                reason: NoticeReason::InteractsWith {
                    pr: PrNumber(9),
                    batch: BatchId(7),
                },
            },
        ]
    }

    #[test]
    fn every_body_begins_with_its_marker() {
        for event in every_event() {
            let marker = event.marker();
            let text = match &event {
                Event::Blocked { on } => format!("blocked {on}"),
                _ => marker.word().to_owned(),
            };
            let body = message(event).body("main", "queue");
            assert_eq!(Marker::of(&body), Some(marker), "{body}");
            assert!(
                body.starts_with(&format!("<!-- dorfq: {text} -->\n")),
                "{body}"
            );
        }
    }

    #[test]
    fn a_blocked_marker_names_its_blocker() {
        let on_pr = Blocker::Pr(PrNumber(9));
        let on_branch = Blocker::Branch("release".to_owned());
        let blocked =
            |on: &Blocker| message(Event::Blocked { on: on.clone() }).body("main", "queue");
        assert!(blocked(&on_pr).starts_with("<!-- dorfq: blocked #9 -->\n"));
        assert!(blocked(&on_branch).starts_with("<!-- dorfq: blocked branch release -->\n"));

        let thread = vec![blocked(&on_pr), "LGTM".to_owned()];
        assert!(told_blocked(&thread, &on_pr));
        assert!(!told_blocked(&thread, &on_branch));
        assert!(!told_blocked(&thread, &Blocker::Pr(PrNumber(10))));
        let renamed = vec![blocked(&Blocker::Branch("release-2".to_owned()))];
        assert!(!told_blocked(&renamed, &on_branch));
        let later = vec![
            blocked(&on_pr),
            message(Event::Withdrawn).body("main", "queue"),
        ];
        assert!(!told_blocked(&later, &on_pr));
        assert!(!told_blocked(&[], &on_pr));
        let bare = vec!["<!-- dorfq: blocked -->\ntext".to_owned()];
        assert!(!told_blocked(&bare, &on_pr));
        let notified = vec![blocked(&on_pr), "<!-- dorfq: notice -->\ntext".to_owned()];
        assert!(told_blocked(&notified, &on_pr));
    }

    #[test]
    fn bodies_carry_what_the_author_needs() {
        let bodies: Vec<String> = every_event()
            .into_iter()
            .map(|e| message(e).body("main", "queue"))
            .collect();
        assert!(
            bodies[0].contains("`queue` label withdraws"),
            "{}",
            bodies[0]
        );
        assert!(
            bodies[0].contains("fast-forwarding `main`"),
            "{}",
            bodies[0]
        );
        assert!(bodies[1].contains(
            "Waiting in the merge queue: own CI pending, mergeability unknown. It joins"
        ));
        assert!(bodies[2].contains("batch 7, branch `dorfq/batch/7`, tip"));
        assert!(bodies[2].contains(TIP));
        assert!(bodies[3].contains(&format!("`main` is now {TIP}")));
        assert!(bodies[4].contains("queue: conflicts with trunk. Merge or rebase onto `main`"));
        assert!(bodies[4].contains("re-add the `queue` label"));
        assert!(bodies[5].contains("conflicts with #9. #9 is ahead"));
        assert!(bodies[5].contains("Once #9 lands, rebase"));
        assert!(
            bodies[6].contains("batch 7 failed. The batch failed its checks on `dorfq/batch/7`")
        );
        assert!(
            bodies[7].contains(
                "Left the merge queue (label removed, pull request closed, or retargeted)."
            )
        );
        assert!(
            bodies[8].contains(
                "Not in the merge queue: its base #9 is not queued. It enters the queue \
                 once #9 does, or retarget it onto `main`."
            ),
            "{}",
            bodies[8]
        );
        assert!(
            bodies[9].contains(
                "Not in the merge queue: it targets `release`, which is neither `main` nor \
                 the branch of a queued pull request. Retarget it onto `main` or onto a \
                 queued pull request's branch."
            ),
            "{}",
            bodies[9]
        );
        assert!(
            bodies[10].contains(
                "conflicts with #9. It does not merge into its base #9. Rebase onto #9's \
                 branch, then re-add the `queue` label."
            ),
            "{}",
            bodies[10]
        );
        assert!(
            bodies[11].contains(
                "Heads-up from the merge queue: this pull request is probably broken against \
                 `main`. Batch 7 (`dorfq/batch/7`) failed and nothing else in it explains \
                 the failure. It stays in the queue"
            ),
            "{}",
            bodies[11]
        );
        assert!(
            bodies[11].contains("rebase onto `main` and let its own checks run again."),
            "{}",
            bodies[11]
        );
        assert!(
            bodies[12].contains(
                "probably passes on its own but fails together with #9, last in batch 7 \
                 (`dorfq/batch/7`). It stays in the queue and is not tested with #9 again"
            ),
            "{}",
            bodies[12]
        );
        assert!(
            bodies[12].contains("rebase onto `main` once #9 lands and fix the interaction."),
            "{}",
            bodies[12]
        );
        assert!(
            bodies[12].contains("the interaction is #9's to fix"),
            "{}",
            bodies[12]
        );
    }

    #[test]
    fn sink_lines_name_the_pull_request_and_event() {
        let lines: Vec<String> = every_event()
            .into_iter()
            .map(|e| message(e).to_string())
            .collect();
        assert_eq!(
            lines,
            [
                "message to pr 12: queued".to_owned(),
                "message to pr 12: held: own CI pending, mergeability unknown".to_owned(),
                "message to pr 12: testing in batch 7".to_owned(),
                format!("message to pr 12: landed as {TIP}"),
                "message to pr 12: ejected: conflicts with trunk".to_owned(),
                "message to pr 12: ejected: conflicts with #9".to_owned(),
                "message to pr 12: ejected: batch 7 failed".to_owned(),
                "message to pr 12: withdrawn".to_owned(),
                "message to pr 12: blocked on #9".to_owned(),
                "message to pr 12: blocked on branch release".to_owned(),
                "message to pr 12: ejected: conflicts with #9".to_owned(),
                "message to pr 12: notice: probably broken against trunk (batch 7)".to_owned(),
                "message to pr 12: notice: probably fails together with #9 (batch 7)".to_owned(),
            ]
        );
    }

    #[test]
    fn markers_are_read_only_at_the_start_of_a_body() {
        assert_eq!(
            Marker::of("<!-- dorfq: queued -->\nhi"),
            Some(Marker::Queued)
        );
        assert_eq!(Marker::of("quoting <!-- dorfq: queued -->"), None);
        assert_eq!(
            Marker::of("<!-- dorfq: blocked -->\nhi"),
            Some(Marker::Blocked)
        );
        assert_eq!(
            Marker::of("<!-- dorfq: blocked branch release -->\nhi"),
            Some(Marker::Blocked)
        );
        assert_eq!(
            Marker::of("<!-- dorfq: notice -->\nhi"),
            Some(Marker::Notice)
        );
        assert_eq!(Marker::of("<!-- dorfq: merged -->"), None);
        assert_eq!(Marker::of("<!-- dorfq: queued"), None);
        assert_eq!(Marker::of("LGTM"), None);
    }

    #[test]
    fn the_last_marked_comment_decides_the_greeting() {
        let body = |m: &str| format!("<!-- dorfq: {m} -->\ntext");
        assert!(needs_greeting(&[]));
        assert!(needs_greeting(&["LGTM".to_owned()]));
        for greeted in ["queued", "held", "testing"] {
            let thread = vec![body("withdrawn"), body(greeted), "LGTM".to_owned()];
            assert!(!needs_greeting(&thread), "{greeted}");
            let notified = vec![body("withdrawn"), body(greeted), body("notice")];
            assert!(!needs_greeting(&notified), "{greeted} then notice");
        }
        for left in ["landed", "ejected", "withdrawn", "blocked"] {
            let thread = vec![body("queued"), body(left), "please re-queue".to_owned()];
            assert!(needs_greeting(&thread), "{left}");
            let notified = vec![body("queued"), body(left), body("notice")];
            assert!(needs_greeting(&notified), "{left} then notice");
        }
    }
}
