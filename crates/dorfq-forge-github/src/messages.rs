//! What the forge tells authors (rule 9): one issue comment per transition
//! the forge tells — what the queue did or decided, and what the author has
//! to do (**The rule** below) — each beginning with a hidden marker so the
//! comment thread is the forge's own journal of what it has said.
//!
//! # The rule
//!
//! The queue tells an author what the queue did or decided, and what the
//! author has to do that GitHub does not already show on the pull request.
//! It says nothing about GitHub's own state — a check pending or failed, a
//! draft, an unmet protection rule, mergeability being recomputed — since
//! GitHub shows each on the pull request and the queue waits on GitHub to
//! report it ready; and nothing about the author's own acts, a label
//! removed or a pull request closed.
//!
//! # Markers
//!
//! The marker is `<!-- dorfq: <event> -->`, with `event` one of `queued`,
//! `held`, `testing`, `landed`, `ejected`, `notice`, `blocked`; a `blocked`
//! marker also names the blocker, `blocked #<n>` or `blocked branch
//! <name>`. A `notice` is the scheduler telling the author what it
//! believes; it is not a transition, the pull request stays queued, and
//! every reading of the thread skips it. A pull request whose last
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
    /// The pull request is in batch `batch`, published at `tip`; `locked`
    /// when its head branch is locked until the batch ends.
    Testing {
        batch: BatchId,
        tip: Sha,
        locked: bool,
    },
    /// The pull request landed with batch `batch`; trunk was fast-forwarded
    /// to `tip`.
    Landed { batch: BatchId, tip: Sha },
    /// The scheduler ejected the pull request, which was based on `base`.
    Ejected { reason: EjectReason, base: PrBase },
    /// The scheduler told the author what it believes; the pull request
    /// stays queued.
    Notice { reason: NoticeReason },
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
            Event::Blocked { .. } => Marker::Blocked,
        }
    }
}

impl Message {
    /// The comment body: the marker line, then the text, in Markdown.
    /// `trunk` and `label` are the configured branch and intent label.
    pub fn body(&self, trunk: &str, label: &str) -> String {
        let text = match &self.event {
            Event::Queued => {
                format!("In the merge queue. Remove the `{label}` label to take it out.")
            }
            Event::Held { holds } => {
                let sentences: Vec<String> = holds.iter().map(held_sentence).collect();
                format!("Waiting: {}", sentences.join(" "))
            }
            Event::Testing { batch, tip, locked } => {
                let mut text = format!(
                    "Testing in batch {batch}: `{branch}` at {tip}.",
                    batch = batch.0,
                    branch = crate::batch_branch(*batch)
                );
                if *locked {
                    text.push_str(&format!(
                        " Its branch is locked until the batch ends; remove the `{label}` \
                         label to push."
                    ));
                }
                text
            }
            Event::Landed { batch, tip } => {
                format!("Landed in batch {}; `{trunk}` is now {tip}.", batch.0)
            }
            Event::Ejected { reason, base } => {
                format!(
                    "Ejected: {reason}. {}",
                    next_step(reason, *base, trunk, label)
                )
            }
            Event::Notice { reason } => notice(reason, trunk),
            Event::Blocked {
                on: Blocker::Pr(pr),
            } => format!(
                "Not queued: its base #{pr} is not. Label #{pr}, or retarget this onto `{trunk}`.",
                pr = pr.0
            ),
            Event::Blocked {
                on: Blocker::Branch(name),
            } => format!(
                "Not queued: it targets `{name}`, which is neither `{trunk}` nor a queued pull \
                 request's branch."
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
            "Rebase onto #{pr}'s branch and re-add the `{label}` label.",
            pr = pr.0
        ),
        EjectReason::Conflicts {
            with: ConflictWith::Trunk,
        } => format!("Rebase onto `{trunk}` and re-add the `{label}` label."),
        EjectReason::Conflicts {
            with: ConflictWith::Pr(pr),
        } => format!(
            "Once #{pr} lands, rebase onto `{trunk}` and re-add the `{label}` label.",
            pr = pr.0
        ),
        EjectReason::BatchFailed { batch } => format!(
            "See the checks on `{branch}`. Re-add the `{label}` label to try again.",
            branch = crate::batch_branch(*batch)
        ),
        _ => format!("Re-add the `{label}` label to try again."),
    }
}

/// What the scheduler believes about a queued pull request, and what the
/// queue does next.
fn notice(reason: &NoticeReason, trunk: &str) -> String {
    match reason {
        NoticeReason::Broken { batch } => format!(
            "Batch {n} (`{branch}`) failed and this pull request is the likely cause. It \
             stays queued, and is ejected if the queue becomes sure; to settle it, rebase \
             onto `{trunk}` and let its checks run.",
            n = batch.0,
            branch = crate::batch_branch(*batch)
        ),
        NoticeReason::InteractsWith { pr, batch } => format!(
            "Batch {n} (`{branch}`) failed; this pull request and #{pr} probably fail only \
             together. They are not batched together again; once #{pr} lands, this one is \
             tested alone and ejected if that fails. To settle it, rebase onto `{trunk}` \
             after #{pr} lands and fix the interaction.",
            pr = pr.0,
            n = batch.0,
            branch = crate::batch_branch(*batch)
        ),
    }
}

/// One sentence for a hold the author is told about.
fn held_sentence(hold: &Hold) -> String {
    match hold {
        Hold::AncestorMoved { pr } => format!("#{} has moved; rebase onto it.", pr.0),
        hold => format!("{hold}."),
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
                holds: BTreeSet::from([Hold::AncestorMoved { pr: PrNumber(9) }]),
            },
            Event::Testing {
                batch: BatchId(7),
                tip: tip.clone(),
                locked: true,
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
            message(Event::Queued).body("main", "queue"),
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
        let texts: Vec<String> = every_event()
            .into_iter()
            .map(|e| {
                let body = message(e).body("main", "queue");
                let (_, text) = body.split_once('\n').unwrap();
                text.to_owned()
            })
            .collect();
        assert_eq!(
            texts,
            [
                "In the merge queue. Remove the `queue` label to take it out.\n".to_owned(),
                "Waiting: #9 has moved; rebase onto it.\n".to_owned(),
                format!(
                    "Testing in batch 7: `dorfq/batch/7` at {TIP}. Its branch is locked until \
                     the batch ends; remove the `queue` label to push.\n"
                ),
                format!("Landed in batch 7; `main` is now {TIP}.\n"),
                "Ejected: conflicts with trunk. Rebase onto `main` and re-add the `queue` \
                 label.\n"
                    .to_owned(),
                "Ejected: conflicts with #9. Once #9 lands, rebase onto `main` and re-add the \
                 `queue` label.\n"
                    .to_owned(),
                "Ejected: batch 7 failed. See the checks on `dorfq/batch/7`. Re-add the \
                 `queue` label to try again.\n"
                    .to_owned(),
                "Not queued: its base #9 is not. Label #9, or retarget this onto `main`.\n"
                    .to_owned(),
                "Not queued: it targets `release`, which is neither `main` nor a queued pull \
                 request's branch.\n"
                    .to_owned(),
                "Ejected: conflicts with #9. Rebase onto #9's branch and re-add the `queue` \
                 label.\n"
                    .to_owned(),
                "Batch 7 (`dorfq/batch/7`) failed and this pull request is the likely cause. \
                 It stays queued, and is ejected if the queue becomes sure; to settle it, \
                 rebase onto `main` and let its checks run.\n"
                    .to_owned(),
                "Batch 7 (`dorfq/batch/7`) failed; this pull request and #9 probably fail \
                 only together. They are not batched together again; once #9 lands, this one \
                 is tested alone and ejected if that fails. To settle it, rebase onto `main` \
                 after #9 lands and fix the interaction.\n"
                    .to_owned(),
            ]
        );
        let unlocked = message(Event::Testing {
            batch: BatchId(7),
            tip: Sha::parse(TIP).unwrap(),
            locked: false,
        })
        .body("main", "queue");
        assert!(!unlocked.contains("locked"), "{unlocked}");
        let other = message(Event::Ejected {
            reason: EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            },
            base: PrBase::Pr(PrNumber(9)),
        })
        .body("main", "queue");
        assert!(other.contains("Rebase onto `main`"), "{other}");
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
                "message to pr 12: held: ancestor #9 moved".to_owned(),
                "message to pr 12: testing in batch 7".to_owned(),
                format!("message to pr 12: landed as {TIP}"),
                "message to pr 12: ejected: conflicts with trunk".to_owned(),
                "message to pr 12: ejected: conflicts with #9".to_owned(),
                "message to pr 12: ejected: batch 7 failed".to_owned(),
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
            let thread = vec![body("landed"), body(greeted), "LGTM".to_owned()];
            assert!(!needs_greeting(&thread), "{greeted}");
            let notified = vec![body("landed"), body(greeted), body("notice")];
            assert!(!needs_greeting(&notified), "{greeted} then notice");
            // An old `withdrawn` comment reads as unmarked: the comment
            // before it decides.
            let withdrawn = vec![body(greeted), body("withdrawn")];
            assert!(!needs_greeting(&withdrawn), "{greeted} then withdrawn");
        }
        assert!(needs_greeting(&[body("withdrawn")]));
        for left in ["landed", "ejected", "blocked"] {
            let thread = vec![body("queued"), body(left), "please re-queue".to_owned()];
            assert!(needs_greeting(&thread), "{left}");
            let notified = vec![body("queued"), body(left), body("notice")];
            assert!(needs_greeting(&notified), "{left} then notice");
        }
    }
}
