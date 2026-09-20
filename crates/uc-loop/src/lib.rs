//! Step loop: perceive (UIA) → reduce → Jev fan-out → validate in code → act → settle.
//!
//! Everything that *decides* lives in this crate, in one place, as the vendor advises:
//! question texts and thresholds in [`consts`], the code-side gate in [`policy`]
//! (Jev advises, code decides), the executor in [`exec`] and the MVP runner in
//! [`runner`]. The runner is single-threaded on purpose (COM/STA for UIA on the
//! caller's thread, a tokio runtime for HTTP); the multi-thread layout from
//! `docs/03-architektura.md` is phase 1.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

pub mod consts {
    /// Universal floor from the confidence-routing pattern: below it, never act.
    pub const CONF_FLOOR: f64 = 0.60;
    /// System keys (Win, Alt+F4) and window-level operations.
    pub const CONF_SYSTEM_KEY: f64 = 0.75;
    /// Irreversible actions: threshold AND a spoken confirmation, always.
    pub const CONF_IRREVERSIBLE: f64 = 0.85;
    /// `goal_reached` must clear this AND `op` must say `done` (at `CONF_FLOOR`) before
    /// the loop stops. Lower than the irreversible bar on purpose: a false `done` costs a
    /// re-run, and the same task measured 0.85 / 0.84 on two runs (`bench/R4-mvp-run.md`).
    pub const DONE_THRESHOLD: f64 = 0.75;
    /// Narrow (region → element) instead of accepting when the top-2 gap is below this.
    pub const TOP2_GAP_MIN: f64 = 0.15;
    /// Escalate after this many consecutive steps with normalised entropy above 0.6.
    pub const ENTROPY_MAX: f64 = 0.60;
    pub const ENTROPY_STRIKES: u32 = 2;
    /// `is_destructive` at or above this blocks the action unless explicitly allowed.
    pub const DESTRUCTIVE_BLOCK: f64 = 0.50;
    /// Consecutive uncertain verdicts before the run stops and asks for help.
    pub const UNCERTAIN_STRIKES: u32 = 2;
    /// Consecutive actions with no visible change before the run stops.
    pub const STALL_STRIKES: u32 = 2;
    pub const MAX_STEPS_DEFAULT: usize = 12;
    /// Max candidates sent to Jev (measured stable to 60).
    pub const MAX_CANDIDATES: usize = 60;
    /// Pop-ups of the target process scanned in addition to the foreground window
    /// (a menu and its sub-menu, a drop-down, an owned dialog).
    pub const MAX_POPUPS: usize = 3;
    /// Name of the synthetic element that stands for an empty spot of the target
    /// window: the only way to say "right-click the background" (desktop → New…).
    pub const BACKGROUND_NAME: &str = "background (empty area)";
    /// Caption / tab strip height skipped when looking for an empty spot, px.
    pub const BACKGROUND_TOP_SKIP: i32 = 48;
    pub const BACKGROUND_MARGIN: i32 = 16;
    /// System Two (OpenRouter chat model) consultations per run: one plan + rescues.
    pub const TWO_MAX_CALLS: u32 = 3;
    /// How long the loop waits for a rescue when Jev is stuck (kill switch and stop
    /// are polled meanwhile). Only then: a flowing loop never waits for System Two.
    pub const TWO_WAIT_MS: u64 = 15_000;
    /// One consultation's HTTP timeout — longer than `TWO_WAIT_MS` on purpose: a plan
    /// is never waited for and is still useful when it lands two steps later, and a
    /// late rescue still contributes its plan/text (its `next` is discarded as stale).
    pub const TWO_TIMEOUT_MS: u64 = 25_000;
    pub const TWO_DEFAULT_MODEL: &str = "google/gemini-2.5-flash-lite";
    /// Settle cap after an action (jev-ultrafast: 50 ms / 2 frames; combobox 200 ms).
    /// MVP sleeps this long; phase 1 replaces it with UIA events + hash polling.
    pub const SETTLE_CAP_MS: u64 = 200;
    /// Pause between focusing a field and typing into it.
    pub const FOCUS_SETTLE_MS: u64 = 40;
    /// Length of a `wait` step.
    pub const WAIT_MS: u64 = 300;
    /// Consecutive `wait` steps before the run stops (waits never count as stalls).
    pub const WAIT_STRIKES: u32 = 3;
    pub const SCROLL_NOTCHES: i32 = 3;
    /// Hedge an in-flight decision after this delay. A hedge can only win when the
    /// first request stalls past `delay + floor` (floor ≈ 267 ms from PL); with a
    /// 350–400 ms delay it won 0/8 times in R2 (max of 110 calls: 425 ms), so this is
    /// a stall guard at ~2×p50, not a tail trimmer (`bench/R2-jev.md`).
    pub const HEDGE_AFTER_MS: u64 = 600;
    pub const DECISION_TIMEOUT_MS: u64 = 1500;

    /// Names on controls that make an action irreversible regardless of what Jev says.
    pub const IRREVERSIBLE_MARKERS: [&str; 20] = [
        "usuń",
        "usun",
        "delete",
        "remove",
        "wyślij",
        "wyslij",
        "send",
        "zapłać",
        "zaplac",
        "pay",
        "kup",
        "buy",
        "formatuj",
        "format",
        "uninstall",
        "odinstaluj",
        "wyczyść",
        "wyczysc",
        "drop",
        "truncate",
    ];

    /// Keys that destroy or send regardless of the control under focus.
    pub const DESTRUCTIVE_KEYS: [&str; 2] = ["delete", "shift+delete"];

    pub fn is_irreversible_name(name: &str) -> bool {
        let n = name.to_lowercase();
        IRREVERSIBLE_MARKERS.iter().any(|m| n.contains(m))
    }

    /// The question bundle. Every step asks all of these in one request — questions to
    /// one state run in parallel and cost only their own tokens.
    pub mod q {
        pub const TARGET: &str =
            "Which element in `elements` should be acted on next to advance `goal`?";
        pub const TARGET_NONE: &str =
            "No listed element advances `goal`; a key, scroll or wait is needed.";
        pub const OP: &str = "What kind of input advances `goal` from this screen?";
        pub const OPS: [(&str, &str); 8] = [
            ("click", "Activate a visible control."),
            (
                "right_click",
                "Open the context menu of a control or of the background.",
            ),
            ("type", "Enter `dictated` text into a text field."),
            ("key", "Press a keyboard shortcut (see `key`)."),
            ("scroll_down", "Content needed is below the visible area."),
            ("scroll_up", "Content needed is above the visible area."),
            ("wait", "The UI is still loading or animating."),
            ("done", "`goal` is already satisfied by the visible state."),
        ];
        pub const KEY: &str = "If a key press is the next step, which one?";
        pub const KEYS: [(&str, &str); 10] = [
            ("enter", "Confirm / activate the default button."),
            ("f2", "Rename the selected item."),
            ("delete", "Delete the selected item (hard to undo)."),
            ("esc", "Cancel / close the dialog or menu."),
            ("tab", "Move focus to the next control."),
            ("ctrl+s", "Save."),
            ("ctrl+z", "Undo the last edit."),
            ("down", "Move the selection down."),
            ("up", "Move the selection up."),
            ("none", "No key press is needed."),
        ];
        pub const GOAL: &str = "Given `last` (the previous action and its effect), is `goal` already satisfied by the visible state (`elements`, `scene.title`)?";
        pub const GOAL_TRUE: &str = "The visible state shows `goal` completed; nothing more to do.";
        pub const GOAL_FALSE: &str = "At least one step of `goal` is still pending.";
        /// Inverted phrasing of `GOAL`; the two are averaged (self-consistency), because a
        /// single noul is a weak signal and P(x) ≠ 1 − P(¬x) on this model.
        pub const GOAL_PENDING: &str =
            "Is there still something left to do before `goal` is complete?";
        pub const GOAL_PENDING_TRUE: &str = "Yes — at least one more input is required.";
        pub const GOAL_PENDING_FALSE: &str = "No — `goal` is done as far as the screen shows.";
        pub const NEEDS_TEXT: &str = "Does the next step toward `goal` require typing new text?";
        pub const NEEDS_TEXT_TRUE: &str =
            "A text field must receive content before `goal` can advance.";
        pub const NEEDS_TEXT_FALSE: &str = "No typing is needed for the next step.";
        pub const DESTRUCTIVE: &str = "Would the most likely next action delete data, send a message, pay, or otherwise be hard to undo?";
        pub const DESTRUCTIVE_TRUE: &str = "The next action is irreversible or destructive.";
        pub const DESTRUCTIVE_FALSE: &str = "The next action is safe and reversible.";
        /// Shortlist round: one independent `noul` per finalist instead of a forced
        /// choice — two valid routes (button vs menu) split a `choice` 50/50 forever,
        /// while each of them gates high on its own.
        pub const OK_PREFIX: &str = "Is acting on this control a correct next step toward `goal`:";
        pub const OK_TRUE: &str = "Yes — it advances `goal` from the current screen.";
        pub const OK_FALSE: &str = "No — wrong control, or not yet.";
    }
}

// ------------------------------------------------------------------ questions

pub mod questions {
    use serde_json::{Map, Value};
    use uc_uia::Element;

    use crate::consts::q;

    /// One `choice` criterion per element (`e0`, `e1`, …) plus `none`. Same shape as the
    /// Python reference (`JevUse/questions.py`) so R2 numbers transfer.
    pub fn criteria(elements: &[Element]) -> Vec<(String, String)> {
        let mut c = Vec::with_capacity(elements.len() + 1);
        for e in elements {
            let mut d = format!("{} „{}”", e.role, e.name);
            if let Some(v) = &e.val {
                d.push_str(&format!(" = „{v}”"));
            }
            if !e.enabled {
                d.push_str(" (disabled)");
            }
            c.push((format!("e{}", e.i), d));
        }
        c.push(("none".to_string(), q::TARGET_NONE.to_string()));
        c
    }

    /// The seven questions of one step, keyed by name.
    pub fn bundle(criteria: &[(String, String)]) -> Map<String, Value> {
        let crit: Vec<(&str, String)> = criteria
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let ops: Vec<(&str, String)> = q::OPS.iter().map(|(k, v)| (*k, v.to_string())).collect();
        let keys: Vec<(&str, String)> = q::KEYS.iter().map(|(k, v)| (*k, v.to_string())).collect();
        let mut m = Map::new();
        m.insert("target".into(), uc_jev::choice(q::TARGET, &crit));
        m.insert("op".into(), uc_jev::choice(q::OP, &ops));
        m.insert("key".into(), uc_jev::choice(q::KEY, &keys));
        m.insert(
            "goal_reached".into(),
            uc_jev::noul(q::GOAL, q::GOAL_TRUE, q::GOAL_FALSE),
        );
        m.insert(
            "goal_pending".into(),
            uc_jev::noul(q::GOAL_PENDING, q::GOAL_PENDING_TRUE, q::GOAL_PENDING_FALSE),
        );
        m.insert(
            "needs_text".into(),
            uc_jev::noul(q::NEEDS_TEXT, q::NEEDS_TEXT_TRUE, q::NEEDS_TEXT_FALSE),
        );
        m.insert(
            "is_destructive".into(),
            uc_jev::noul(q::DESTRUCTIVE, q::DESTRUCTIVE_TRUE, q::DESTRUCTIVE_FALSE),
        );
        m
    }

    pub fn compile(elements: &[Element]) -> uc_jev::Compiled {
        uc_jev::Compiled::new(&bundle(&criteria(elements)))
    }
}

// ------------------------------------------------------------------ policy

pub mod policy {
    use serde::Serialize;
    use uc_uia::Element;

    use crate::consts::*;

    /// What the executor can do. Coordinates are physical pixels.
    #[derive(Clone, Debug, PartialEq, Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Action {
        Click {
            target: usize,
            name: String,
            x: i32,
            y: i32,
            /// Context menu instead of activation.
            right: bool,
        },
        Type {
            text: String,
            focus: Option<(i32, i32)>,
            target_name: Option<String>,
        },
        Key {
            key: String,
        },
        Scroll {
            notches: i32,
            at: Option<(i32, i32)>,
        },
        Wait,
    }

    #[derive(Clone, Debug, PartialEq, Serialize)]
    #[serde(tag = "verdict", rename_all = "snake_case")]
    pub enum Verdict {
        Done,
        Act {
            action: Action,
        },
        /// `narrow`: candidate ids worth one narrower re-ask (the shortlist pattern).
        Uncertain {
            reason: String,
            narrow: Option<Vec<String>>,
        },
        NeedsText,
        Blocked {
            reason: String,
        },
    }

    /// The numbers one step reads out of a [`uc_jev::Decision`] — everything the gate
    /// needs, nothing else, so the gate is testable without a network.
    #[derive(Clone, Debug, Default, Serialize)]
    pub struct Signals {
        pub target: String,
        pub target_name: Option<String>,
        /// Top-3 `(id, probability)` of `target`, for the duplicate-name merge.
        pub target_top: Vec<(String, f64)>,
        pub target_conf: f64,
        pub target_gap: f64,
        pub target_entropy: f64,
        pub op: String,
        pub op_conf: f64,
        pub key: String,
        pub key_conf: f64,
        /// Mean of `goal_reached` and `1 − goal_pending` (two phrasings, one call).
        pub goal_reached: f64,
        pub goal_a: f64,
        pub goal_pending: f64,
        pub needs_text: f64,
        pub is_destructive: f64,
    }

    /// `e12` → `12`.
    pub fn parse_target(id: &str) -> Option<usize> {
        id.strip_prefix('e').and_then(|n| n.parse().ok())
    }

    pub fn find(els: &[Element], i: usize) -> Option<&Element> {
        els.iter().find(|e| e.i == i)
    }

    pub fn describe(e: &Element) -> String {
        format!("{} „{}”", e.role, e.name)
    }

    impl Signals {
        pub fn from_decision(d: &uc_jev::Decision, els: &[Element]) -> Self {
            let (target, target_conf) = d
                .choice("target")
                .map(|(c, p)| (c.to_string(), p))
                .unwrap_or_else(|| ("none".to_string(), 0.0));
            let target_name = parse_target(&target)
                .and_then(|i| find(els, i))
                .map(describe);
            let (op, op_conf) = d
                .choice("op")
                .map(|(c, p)| (c.to_string(), p))
                .unwrap_or_else(|| ("wait".to_string(), 0.0));
            let (key, key_conf) = d
                .choice("key")
                .map(|(c, p)| (c.to_string(), p))
                .unwrap_or_else(|| ("none".to_string(), 0.0));
            let goal_a = d.noul("goal_reached").unwrap_or(0.0);
            let goal_pending = d.noul("goal_pending").unwrap_or(1.0);
            Self {
                target,
                target_name,
                target_top: d.top("target", 3),
                target_conf,
                target_gap: d.top2_gap("target").unwrap_or(0.0),
                target_entropy: d.entropy_norm("target").unwrap_or(1.0),
                op,
                op_conf,
                key,
                key_conf,
                goal_reached: (goal_a + (1.0 - goal_pending)) / 2.0,
                goal_a,
                goal_pending,
                needs_text: d.noul("needs_text").unwrap_or(0.0),
                is_destructive: d.noul("is_destructive").unwrap_or(0.0),
            }
        }
    }

    fn unsure(reason: String) -> Verdict {
        Verdict::Uncertain {
            reason,
            narrow: None,
        }
    }

    /// The two strongest *listed* candidates, if the call is between them (not `none`).
    fn shortlist(s: &Signals) -> Option<Vec<String>> {
        let ids: Vec<String> = s
            .target_top
            .iter()
            .take(2)
            .map(|(id, _)| id.clone())
            .filter(|id| id != "none")
            .collect();
        (ids.len() == 2).then_some(ids)
    }

    /// The gate. Pure code over the signals: thresholds, agreement between independent
    /// questions, the irreversible list. Jev never gets the last word on an action.
    pub fn judge(
        s: &Signals,
        els: &[Element],
        dictated: Option<&str>,
        allow_irreversible: bool,
    ) -> Verdict {
        let goal_done = s.goal_reached >= DONE_THRESHOLD;
        if goal_done && s.op == "done" {
            if s.op_conf < CONF_FLOOR {
                return unsure(format!(
                    "done at op confidence {:.2} < {CONF_FLOOR}",
                    s.op_conf
                ));
            }
            return Verdict::Done;
        }
        if goal_done || s.op == "done" {
            return unsure(format!(
                "goal_reached {:.2} and op {} disagree",
                s.goal_reached, s.op
            ));
        }
        if s.op_conf < CONF_FLOOR {
            return unsure(format!("op {} at {:.2} < {CONF_FLOOR}", s.op, s.op_conf));
        }
        let target_el = parse_target(&s.target).and_then(|i| find(els, i));
        let (target_conf, target_gap) = (s.target_conf, s.target_gap);
        let target_ok = target_el.is_some_and(|e| e.enabled)
            && target_conf >= CONF_FLOOR
            && target_gap >= TOP2_GAP_MIN;
        let unsure_target = |reason: String| Verdict::Uncertain {
            reason,
            narrow: shortlist(s),
        };
        let destructive = s.is_destructive >= DESTRUCTIVE_BLOCK
            || target_el.is_some_and(|e| is_irreversible_name(&e.name))
            || (s.op == "key" && DESTRUCTIVE_KEYS.contains(&s.key.as_str()));
        let blocked = |what: &str| {
            Verdict::Blocked {
            reason: format!(
                "{what} looks irreversible (is_destructive {:.2}, target {}); pass --allow-irreversible",
                s.is_destructive,
                s.target_name.as_deref().unwrap_or("none")
            ),
        }
        };
        match s.op.as_str() {
            "wait" => Verdict::Act {
                action: Action::Wait,
            },
            "scroll_down" | "scroll_up" => Verdict::Act {
                action: Action::Scroll {
                    notches: if s.op == "scroll_down" {
                        SCROLL_NOTCHES
                    } else {
                        -SCROLL_NOTCHES
                    },
                    at: target_el.map(Element::center),
                },
            },
            "key" => {
                if s.key == "none" || s.key_conf < CONF_FLOOR {
                    return unsure(format!("key {} at {:.2}", s.key, s.key_conf));
                }
                if destructive {
                    if !allow_irreversible {
                        return blocked(&format!("key {}", s.key));
                    }
                    if s.key_conf < CONF_IRREVERSIBLE {
                        return unsure(format!(
                            "irreversible key {} at {:.2} < {CONF_IRREVERSIBLE}",
                            s.key, s.key_conf
                        ));
                    }
                }
                Verdict::Act {
                    action: Action::Key { key: s.key.clone() },
                }
            }
            "type" => {
                let Some(text) = dictated else {
                    return Verdict::NeedsText;
                };
                if destructive {
                    if !allow_irreversible {
                        return blocked("typing here");
                    }
                    if s.op_conf < CONF_IRREVERSIBLE {
                        return unsure(format!(
                            "irreversible typing at op confidence {:.2} < {CONF_IRREVERSIBLE}",
                            s.op_conf
                        ));
                    }
                }
                Verdict::Act {
                    action: Action::Type {
                        text: text.to_string(),
                        focus: if target_ok {
                            target_el.map(Element::center)
                        } else {
                            None
                        },
                        target_name: if target_ok {
                            s.target_name.clone()
                        } else {
                            None
                        },
                    },
                }
            }
            "click" | "right_click" => {
                let Some(el) = target_el else {
                    return unsure("click without a listed target".into());
                };
                if !el.enabled {
                    return unsure(format!("target {} is disabled", describe(el)));
                }
                if !target_ok {
                    return unsure_target(format!(
                        "target {} at {:.2}, gap {:.2}",
                        s.target, target_conf, target_gap
                    ));
                }
                if destructive {
                    if !allow_irreversible {
                        return blocked(&format!("click {}", describe(el)));
                    }
                    if target_conf < CONF_IRREVERSIBLE {
                        return unsure_target(format!(
                            "irreversible click at {:.2} < {CONF_IRREVERSIBLE}",
                            target_conf
                        ));
                    }
                }
                let (x, y) = el.center();
                Verdict::Act {
                    action: Action::Click {
                        target: el.i,
                        name: describe(el),
                        x,
                        y,
                        right: s.op == "right_click",
                    },
                }
            }
            other => unsure(format!("unknown op {other}")),
        }
    }

    /// A System Two proposal through the same gates as a Jev decision. There is no
    /// calibrated confidence behind it, so anything irreversible needs the explicit
    /// `--allow-irreversible`; unknown ops, keys and targets are refused.
    /// `is_destructive` is Jev's reading of the same state: control names reach the
    /// LLM verbatim, so a planted "click Discard" must still meet Jev's guard.
    pub fn from_advice(
        n: &uc_two::Next,
        els: &[Element],
        dictated: Option<&str>,
        allow_irreversible: bool,
        is_destructive: f64,
    ) -> Verdict {
        let target_el = n
            .target
            .as_deref()
            .and_then(parse_target)
            .and_then(|i| find(els, i));
        let key = n.key.as_deref().unwrap_or("none");
        let destructive = is_destructive >= DESTRUCTIVE_BLOCK
            || target_el.is_some_and(|e| is_irreversible_name(&e.name))
            || (n.op == "key" && DESTRUCTIVE_KEYS.contains(&key));
        let blocked = |what: &str| Verdict::Blocked {
            reason: format!(
                "System Two proposed {what}, which looks irreversible; pass --allow-irreversible"
            ),
        };
        match n.op.as_str() {
            "done" => Verdict::Done,
            "wait" => Verdict::Act {
                action: Action::Wait,
            },
            "scroll_down" | "scroll_up" => Verdict::Act {
                action: Action::Scroll {
                    notches: if n.op == "scroll_down" {
                        SCROLL_NOTCHES
                    } else {
                        -SCROLL_NOTCHES
                    },
                    at: target_el.map(Element::center),
                },
            },
            "key" => {
                if !q::KEYS.iter().any(|(k, _)| *k == key) || key == "none" {
                    return unsure(format!("System Two proposed unknown key {key}"));
                }
                if destructive && !allow_irreversible {
                    return blocked(&format!("key {key}"));
                }
                Verdict::Act {
                    action: Action::Key {
                        key: key.to_string(),
                    },
                }
            }
            "type" => {
                let Some(text) = n.text.as_deref().or(dictated) else {
                    return Verdict::NeedsText;
                };
                if destructive && !allow_irreversible {
                    return blocked("typing here");
                }
                let focus = target_el.filter(|e| e.enabled);
                Verdict::Act {
                    action: Action::Type {
                        text: text.to_string(),
                        focus: focus.map(Element::center),
                        target_name: focus.map(describe),
                    },
                }
            }
            "click" | "right_click" => {
                let Some(el) = target_el else {
                    return unsure("System Two proposed a click without a listed target".into());
                };
                if !el.enabled {
                    return unsure(format!("target {} is disabled", describe(el)));
                }
                if destructive && !allow_irreversible {
                    return blocked(&format!("click {}", describe(el)));
                }
                let (x, y) = el.center();
                Verdict::Act {
                    action: Action::Click {
                        target: el.i,
                        name: describe(el),
                        x,
                        y,
                        right: n.op == "right_click",
                    },
                }
            }
            other => unsure(format!("System Two proposed unknown op {other}")),
        }
    }
}

// ------------------------------------------------------------------ exec

pub mod exec {
    use std::time::Duration;

    use uc_input::Button;

    use crate::consts::{FOCUS_SETTLE_MS, WAIT_MS};
    use crate::policy::Action;

    /// Inject one action. Every branch is a single `SendInput` batch (plus a clipboard
    /// paste for long text); no allocation happens between the call and the syscall
    /// beyond what `uc-input` already does.
    pub fn perform(a: &Action) -> Result<(), uc_input::InputError> {
        match a {
            Action::Click { x, y, right, .. } => {
                uc_input::click(*x, *y, if *right { Button::Right } else { Button::Left }, 1)?;
            }
            Action::Type { text, focus, .. } => {
                if let Some((x, y)) = focus {
                    uc_input::click(*x, *y, Button::Left, 1)?;
                    std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));
                }
                uc_input::type_auto(text)?;
            }
            Action::Key { key } => {
                let parts: Vec<&str> = key.split('+').collect();
                if parts.len() == 1 {
                    uc_input::press(parts[0])?;
                } else {
                    uc_input::hotkey(&parts)?;
                }
            }
            Action::Scroll { notches, at } => {
                uc_input::scroll(*notches, *at)?;
            }
            Action::Wait => std::thread::sleep(Duration::from_millis(WAIT_MS)),
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ text helpers

/// The first quoted fragment of a goal — „…”, "…", '…' or «…» — is the text to type.
/// Jev never generates text; it comes from the user (typed here, dictated later).
pub fn extract_quoted(goal: &str) -> Option<String> {
    const PAIRS: [(char, char); 5] = [('„', '”'), ('„', '"'), ('"', '"'), ('\'', '\''), ('«', '»')];
    let mut best: Option<(usize, String)> = None;
    for (open, close) in PAIRS {
        // An apostrophe is a quote only at a word boundary: "Don't save" has none.
        let strict = open == '\'';
        let Some(start) = goal
            .match_indices(open)
            .map(|(i, _)| i)
            .find(|&i| !strict || at_word_start(goal, i))
        else {
            continue;
        };
        let inner_start = start + open.len_utf8();
        let Some(end) = goal[inner_start..]
            .match_indices(close)
            .map(|(i, _)| inner_start + i)
            .find(|&i| !strict || at_word_end(goal, i + close.len_utf8()))
        else {
            continue;
        };
        let text = goal[inner_start..end].trim();
        if text.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(s, _)| start < *s) {
            best = Some((start, text.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// A quote character opens a quotation only after start-of-text or a non-letter…
fn at_word_start(s: &str, i: usize) -> bool {
    s[..i]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric())
}

/// …and closes one only before end-of-text or a non-letter.
fn at_word_end(s: &str, i: usize) -> bool {
    s[i..].chars().next().is_none_or(|c| !c.is_alphanumeric())
}

fn slug(goal: &str) -> String {
    let mut s: String = goal
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').chars().take(40).collect()
}

fn ts_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ------------------------------------------------------------------ runner

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("UI Automation init failed: {0}")]
    Uia(String),
    #[error("no foreground window")]
    NoForeground,
    #[error(transparent)]
    Jev(#[from] uc_jev::JevError),
    #[error(transparent)]
    Input(#[from] uc_input::InputError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("System Two: {0}")]
    Two(String),
}

/// One step's view of the target: the foreground window plus its pop-ups, reduced.
pub struct Perception {
    pub scan: uc_uia::Scan,
    pub reduced: Vec<uc_uia::Element>,
    /// Pop-up windows of the target process merged into `scan` (menus, drop-downs,
    /// owned dialogs).
    pub popups: usize,
}

/// Scan `scene`'s window and every pop-up of its process, then reduce. Menus,
/// drop-downs and owned dialogs live in their own top-level windows that never take
/// the foreground, so a scan of the foreground alone would miss them; the viewport
/// grows to cover them because a context menu often hangs outside the window.
pub fn perceive(
    scanner: &uc_uia::UiaScanner,
    scene: &uc_win32::Scene,
    include_context: bool,
    max_n: usize,
) -> Perception {
    let mut scan = scanner.scan_hwnd(scene.hwnd(), include_context);
    let mut viewport = scene.rect;
    let mut popups = 0usize;
    // Pop-up elements go *first*: `reduce` dedupes on (role, name) keeping the first
    // occurrence, and a menu's "Delete" must win over a same-named ribbon button.
    let mut front: Vec<uc_uia::Element> = Vec::new();
    for (h, r) in uc_win32::popups_of(scene.pid, scene.hwnd(), consts::MAX_POPUPS) {
        let extra = scanner.scan_hwnd(h, include_context);
        if extra.elements.is_empty() {
            continue;
        }
        popups += 1;
        scan.raw_count += extra.raw_count;
        scan.total_ms += extra.total_ms;
        viewport = uc_win32::rect_union(viewport, r);
        front.extend(extra.elements);
    }
    if !front.is_empty() {
        front.append(&mut scan.elements);
        scan.elements = front;
    }
    let mut reduced = uc_uia::reduce(
        &scan.elements,
        uc_uia::ReduceOpts {
            max_n,
            near: Some(scene.cursor),
            viewport: Some(viewport),
            ..Default::default()
        },
    );
    // The background is a target too (context menu of the desktop or of an empty
    // canvas); UIA has no element for it, so one is synthesised at a free spot of the
    // main window — after `reduce`, so the candidate cap never drops it.
    if let Some((x, y)) = empty_spot(scene.rect, &reduced) {
        reduced.push(uc_uia::Element {
            i: reduced.len(),
            role: "pane".into(),
            name: consts::BACKGROUND_NAME.into(),
            bbox: [x - 8, y - 8, 16, 16],
            enabled: true,
            val: None,
            focused: false,
            auto_id: None,
        });
    }
    Perception {
        scan,
        reduced,
        popups,
    }
}

/// A point inside `rect` (below the caption, inside the margins) that no element
/// covers — the centre first, then a coarse grid ordered by distance from the centre.
/// `None` when the window is fully covered (a maximised document, a list view).
pub fn empty_spot(rect: uc_win32::Rect, els: &[uc_uia::Element]) -> Option<(i32, i32)> {
    let [rx, ry, rw, rh] = rect;
    let m = consts::BACKGROUND_MARGIN;
    let (x0, y0) = (rx + m, ry + consts::BACKGROUND_TOP_SKIP);
    let (x1, y1) = (rx + rw - m, ry + rh - m);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let covered = |x: i32, y: i32| {
        els.iter().any(|e| {
            let [ex, ey, ew, eh] = e.bbox;
            x >= ex - 12 && x < ex + ew + 12 && y >= ey - 12 && y < ey + eh + 12
        })
    };
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    if !covered(cx, cy) {
        return Some((cx, cy));
    }
    let step = ((x1 - x0) / 12).max(48);
    let mut grid: Vec<(i32, i32)> = Vec::new();
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            grid.push((x, y));
            x += step;
        }
        y += step;
    }
    grid.sort_by_key(|(x, y)| (x - cx).abs() + (y - cy).abs());
    grid.into_iter().find(|&(x, y)| !covered(x, y))
}

#[derive(Clone, Debug)]
pub struct RunOpts {
    /// Inject input. Off = show the first decision and stop (preview).
    pub act: bool,
    pub max_steps: usize,
    pub allow_irreversible: bool,
    /// Text for `type` steps.
    pub dictated: Option<String>,
    /// Directory for the JSONL ledger (one line per step + a summary line).
    pub ledger_dir: Option<PathBuf>,
    /// `None` = discover (vendor first).
    pub provider: Option<uc_jev::Provider>,
    /// Only act while this process owns the foreground window (`None` = lock onto
    /// whatever is in front at the first step). Dialogs of the same process pass.
    pub target_pid: Option<u32>,
    /// The window the run started on (`None` = the foreground window at step 1);
    /// when it stops existing the run ends with [`Outcome::TargetGone`].
    pub target_hwnd: Option<isize>,
    /// Cooperative stop from another thread (a Stop button); checked wherever the
    /// kill switch is.
    pub stop: Option<Arc<AtomicBool>>,
    /// System Two: an OpenRouter chat model consulted beside the loop (plan at the
    /// start, rescue when Jev is stuck, text on demand). `None` = Jev only.
    pub two: Option<uc_two::Config>,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            act: false,
            max_steps: consts::MAX_STEPS_DEFAULT,
            allow_irreversible: false,
            dictated: None,
            ledger_dir: None,
            provider: None,
            target_pid: None,
            target_hwnd: None,
            stop: None,
            two: None,
        }
    }
}

/// One step, as printed and as written to the ledger.
#[derive(Clone, Debug, Serialize)]
pub struct StepRecord {
    pub step: usize,
    pub ts_unix: u64,
    pub app: String,
    pub title: String,
    pub raw_elements: usize,
    /// Pop-up windows of the target process (menus, drop-downs, dialogs) merged into
    /// the scan.
    pub popups: usize,
    /// The sub-goal Jev was asked about (`k/n: text`) when a System Two plan is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subgoal: Option<String>,
    /// System Two consultations applied or received during this step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub two: Vec<uc_two::Note>,
    pub sent_elements: usize,
    pub est_tokens: usize,
    pub tree_hash: u64,
    /// `None` on the first step; afterwards whether the hash moved since the last action.
    pub changed: Option<bool>,
    pub scan_ms: f64,
    pub jev_ms: f64,
    pub act_ms: f64,
    pub total_ms: f64,
    pub input_tokens: u64,
    pub cost_usd: f64,
    pub hedged_winner: u8,
    /// A second, top-2-only ask happened inside this step.
    pub narrowed: bool,
    /// The signals of the first ask when the step was narrowed (`signals` = final).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signals_first: Option<policy::Signals>,
    pub signals: policy::Signals,
    pub verdict: policy::Verdict,
    pub executed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Preview mode: the first action was shown, nothing injected.
    Preview,
    Budget,
    Uncertain(String),
    NeedsText,
    Blocked(String),
    Stalled,
    /// Ctrl+Alt+K.
    Killed,
    /// The caller asked to stop (`RunOpts::stop`).
    Stopped,
    /// Another process took the foreground; nothing was injected into it.
    FocusLost(String),
    /// The window the run started on no longer exists (closed by the last action or
    /// by someone else); code cannot tell whether that was the goal, so it is reported
    /// neutrally and the caller decides.
    TargetGone,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    pub goal: String,
    pub outcome: Outcome,
    pub steps: usize,
    pub elapsed_ms: f64,
    pub jev_calls: u64,
    pub cost_usd: f64,
    pub ledger: Option<PathBuf>,
    pub two_model: Option<String>,
    pub two_calls: u32,
    pub two_cost_usd: f64,
    /// The plan System Two produced, if any was adopted.
    pub plan: Option<Vec<String>>,
}

pub type StepHook = Box<dyn FnMut(&StepRecord)>;

/// The MVP loop. Create on the thread that will run it (UIA initialises COM/STA there).
pub struct Runner {
    opts: RunOpts,
    scanner: uc_uia::UiaScanner,
    rt: tokio::runtime::Runtime,
    client: uc_jev::Client,
    /// Called after every step, before the ledger write.
    pub on_step: Option<StepHook>,
    two: Option<uc_two::Advisor>,
}

impl Runner {
    pub fn new(opts: RunOpts) -> Result<Self, LoopError> {
        let scanner = uc_uia::UiaScanner::new().map_err(|e| LoopError::Uia(e.to_string()))?;
        let mut cfg = match opts.provider {
            Some(p) => uc_jev::Config::for_provider(p)?,
            None => uc_jev::Config::discover()?,
        };
        cfg.timeout = Duration::from_millis(consts::DECISION_TIMEOUT_MS);
        cfg.hedge_after = Some(Duration::from_millis(consts::HEDGE_AFTER_MS));
        let client = uc_jev::Client::new(cfg)?;
        let rt = tokio::runtime::Runtime::new()?;
        let two = match opts.two.clone() {
            Some(cfg) => {
                Some(uc_two::Advisor::spawn(cfg).map_err(|e| LoopError::Two(e.to_string()))?)
            }
            None => None,
        };
        Ok(Self {
            opts,
            scanner,
            rt,
            client,
            on_step: None,
            two,
        })
    }

    pub fn provider(&self) -> uc_jev::Provider {
        self.client.provider()
    }

    fn stop_requested(&self) -> bool {
        stop_set(&self.opts.stop)
    }

    /// System Two's model id, when enabled.
    pub fn two_model(&self) -> Option<&str> {
        self.two.as_ref().map(|t| t.model())
    }

    /// Open the HTTP/2 connection with one tiny decision; returns its latency in ms.
    pub fn warm(&self) -> Result<f64, LoopError> {
        Ok(self.rt.block_on(self.client.warm())?)
    }

    pub fn run(&mut self, goal: &str) -> Result<RunSummary, LoopError> {
        let t_run = Instant::now();
        let mut ledger = match &self.opts.ledger_dir {
            Some(dir) => Some(open_ledger(dir, goal)?),
            None => None,
        };
        let mut dictated = self.opts.dictated.clone();
        let stop_flag = self.opts.stop.clone();
        let allow_irreversible = self.opts.allow_irreversible;
        // System Two's plan: `goal` becomes the current sub-goal until the last is done.
        let mut plan: Option<Vec<String>> = None;
        let mut plan_i = 0usize;
        let mut last: Option<Value> = None;
        let mut last_hash: Option<u64> = None;
        let mut uncertain = 0u32;
        let mut entropy_strikes = 0u32;
        let mut stall = 0u32;
        let mut waits = 0u32;
        let mut last_was_wait = false;
        let mut steps = 0usize;
        let mut jev_calls = 0u64;
        let mut cost = 0.0f64;
        let mut locked_pid = self.opts.target_pid;
        let mut target_hwnd = self.opts.target_hwnd;

        let outcome = loop {
            if steps >= self.opts.max_steps {
                break Outcome::Budget;
            }
            if uc_win32::kill_switch_pressed() {
                break Outcome::Killed;
            }
            if self.stop_requested() {
                break Outcome::Stopped;
            }
            steps += 1;
            let t0 = Instant::now();

            // 1. Perceive: coarse scene in µs, UIA tree in ms (the app's provider decides).
            let scene = uc_win32::scene().ok_or(LoopError::NoForeground)?;
            if let Some(h) = target_hwnd {
                if !uc_win32::is_window(uc_win32::HWND(h as *mut core::ffi::c_void)) {
                    break Outcome::TargetGone;
                }
            } else {
                target_hwnd = Some(scene.hwnd);
            }
            match locked_pid {
                None => locked_pid = Some(scene.pid),
                Some(pid) if pid != scene.pid => {
                    break Outcome::FocusLost(format!(
                        "foreground is now {} (pid {}), locked on pid {pid}",
                        scene.app, scene.pid
                    ));
                }
                Some(_) => {}
            }
            let Perception {
                scan,
                reduced,
                popups,
            } = perceive(&self.scanner, &scene, false, consts::MAX_CANDIDATES);
            let hash = uc_uia::tree_hash(&reduced);
            let scan_ms = ms(t0);

            // 2. Did the last action change anything? Code compares states, not Jev.
            let changed = last_hash.map(|h| h != hash);
            match changed {
                Some(false) => {
                    if !last_was_wait {
                        stall += 1;
                    }
                    if let Some(l) = last.as_mut() {
                        l["effect"] = json!("no visible change");
                    }
                }
                Some(true) => {
                    stall = 0;
                    if let Some(l) = last.as_mut() {
                        l["effect"] = json!("screen changed");
                    }
                }
                None => {}
            }
            if stall >= consts::STALL_STRIKES {
                break Outcome::Stalled;
            }

            // 3. Decide: one request, six questions.
            let goal_now: String = plan
                .as_ref()
                .map(|p| p[plan_i].clone())
                .unwrap_or_else(|| goal.to_string());
            let subgoal_label = plan
                .as_ref()
                .map(|p| format!("{}/{}: {}", plan_i + 1, p.len(), p[plan_i]));
            let state = uc_uia::GuiState {
                goal: &goal_now,
                scene: &scene,
                elements: &reduced,
                last: last.clone(),
                dictated: dictated.as_deref(),
                plan: plan
                    .as_ref()
                    .map(|p| json!({"overall": goal, "steps": p, "current": plan_i})),
            };
            let est_tokens = state.estimate_tokens();
            let state_bytes = serde_json::to_vec(&state)?;
            // System Two, between the lines: the plan request goes out with the first
            // state and is never waited for; the loop keeps deciding with Jev.
            if steps == 1 {
                if let Some(two) = self.two.as_mut() {
                    two.ask(uc_two::Request {
                        kind: uc_two::Kind::Plan,
                        tag: steps as u64,
                        goal: goal.to_string(),
                        subgoal: None,
                        state: serde_json::from_slice(&state_bytes)?,
                        jev: None,
                        why: None,
                        need_text: dictated.is_none(),
                    });
                }
            }
            let compiled = questions::compile(&reduced);
            let t_jev = Instant::now();
            let decision = self
                .rt
                .block_on(self.client.decide(&state_bytes, &compiled))?;
            let mut jev_ms = ms(t_jev);
            jev_calls += 1;
            cost += decision.cost_usd;

            // 4. Gate in code — and on a close call between listed candidates, ask once
            //    more over the top two only (shortlist pattern), still inside this step.
            let mut step_tokens = decision.usage.input_tokens;
            let mut step_cost = decision.cost_usd;
            let mut signals = policy::Signals::from_decision(&decision, &reduced);
            let mut verdict = policy::judge(
                &signals,
                &reduced,
                dictated.as_deref(),
                self.opts.allow_irreversible,
            );
            let mut narrowed = false;
            let mut signals_first: Option<policy::Signals> = None;
            if let policy::Verdict::Uncertain {
                narrow: Some(ids), ..
            } = &verdict
            {
                let mut q = serde_json::Map::new();
                for id in ids {
                    if let Some(el) =
                        policy::parse_target(id).and_then(|i| policy::find(&reduced, i))
                    {
                        q.insert(
                            format!("ok_{id}"),
                            uc_jev::noul(
                                &format!(
                                    "{} {} ({id})",
                                    consts::q::OK_PREFIX,
                                    policy::describe(el)
                                ),
                                consts::q::OK_TRUE,
                                consts::q::OK_FALSE,
                            ),
                        );
                    }
                }
                let compiled2 = uc_jev::Compiled::new(&q);
                let t2 = Instant::now();
                let d2 = self
                    .rt
                    .block_on(self.client.decide(&state_bytes, &compiled2))?;
                jev_ms += ms(t2);
                jev_calls += 1;
                cost += d2.cost_usd;
                step_tokens += d2.usage.input_tokens;
                step_cost += d2.cost_usd;
                let mut scored: Vec<(String, f64)> = ids
                    .iter()
                    .map(|id| (id.clone(), d2.noul(&format!("ok_{id}")).unwrap_or(0.0)))
                    .collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let mut gated = signals.clone();
                if let Some((id, p)) = scored.first() {
                    gated.target = id.clone();
                    gated.target_conf = *p;
                    // Resolved by independent gates, not by a margin between them.
                    gated.target_gap = 1.0;
                    gated.target_entropy = 0.0;
                    gated.target_name = policy::parse_target(id)
                        .and_then(|i| policy::find(&reduced, i))
                        .map(policy::describe);
                    gated.target_top = scored.clone();
                }
                signals_first = Some(std::mem::replace(&mut signals, gated));
                verdict = policy::judge(
                    &signals,
                    &reduced,
                    dictated.as_deref(),
                    self.opts.allow_irreversible,
                );
                narrowed = true;
            }

            // 4b. System Two, between the lines: fold in whatever arrived meanwhile (a
            //     plan, text — a late rescue's `next` is stale and never executed);
            //     when Jev is stuck, or needs text nobody dictated, ask for a rescue and
            //     wait for *that* reply, since the step would otherwise end empty anyway.
            let mut two_notes: Vec<uc_two::Note> = Vec::new();
            let mut two_outcome: Option<Outcome> = None;
            let mut two_decided = false;
            if let Some(two) = self.two.as_mut() {
                while let Some(reply) = two.try_recv() {
                    two_notes.push(apply_reply(
                        &reply,
                        goal,
                        &mut plan,
                        &mut plan_i,
                        &mut dictated,
                        false,
                    ));
                }
                let needs_text = matches!(verdict, policy::Verdict::NeedsText);
                let stuck = matches!(verdict, policy::Verdict::Uncertain { .. })
                    || (needs_text && dictated.is_none());
                if stuck && two.remaining() > 0 {
                    let why = match &verdict {
                        policy::Verdict::Uncertain { reason, .. } => reason.clone(),
                        _ => "the goal needs text and none was dictated".to_string(),
                    };
                    let tag = steps as u64;
                    let asked = two.ask(uc_two::Request {
                        kind: uc_two::Kind::Rescue,
                        tag,
                        goal: goal.to_string(),
                        subgoal: plan.as_ref().map(|p| p[plan_i].clone()),
                        state: serde_json::from_slice(&state_bytes)?,
                        jev: Some(json!({
                            "target_top": signals.target_top,
                            "op": signals.op,
                            "op_conf": signals.op_conf,
                            "goal_reached": signals.goal_reached,
                            "needs_text": signals.needs_text,
                        })),
                        why: Some(why),
                        need_text: needs_text || signals.needs_text >= 0.5,
                    });
                    if asked {
                        let t_wait = Instant::now();
                        let deadline = t_wait + Duration::from_millis(consts::TWO_WAIT_MS);
                        let mut reply = None;
                        while Instant::now() < deadline {
                            if uc_win32::kill_switch_pressed() {
                                two_outcome = Some(Outcome::Killed);
                                break;
                            }
                            if stop_set(&stop_flag) {
                                two_outcome = Some(Outcome::Stopped);
                                break;
                            }
                            match two.wait(Duration::from_millis(100)) {
                                uc_two::Wait::Reply(r)
                                    if r.kind == uc_two::Kind::Rescue && r.tag == tag =>
                                {
                                    reply = Some(r);
                                    break;
                                }
                                // The worker is FIFO: a plan still in flight answers
                                // first. Take it and keep waiting for ours.
                                uc_two::Wait::Reply(r) => two_notes.push(apply_reply(
                                    &r,
                                    goal,
                                    &mut plan,
                                    &mut plan_i,
                                    &mut dictated,
                                    false,
                                )),
                                uc_two::Wait::Timeout => {}
                                uc_two::Wait::Gone => break,
                            }
                        }
                        match reply {
                            Some(r) => {
                                let mut note = apply_reply(
                                    &r,
                                    goal,
                                    &mut plan,
                                    &mut plan_i,
                                    &mut dictated,
                                    true,
                                );
                                let next_step = r.advice.as_ref().ok().and_then(|a| a.next.clone());
                                if let Some(n) = next_step {
                                    let v = policy::from_advice(
                                        &n,
                                        &reduced,
                                        dictated.as_deref(),
                                        allow_irreversible,
                                        signals.is_destructive,
                                    );
                                    match &v {
                                        policy::Verdict::Act { .. } | policy::Verdict::Done => {
                                            note.applied = true;
                                            two_decided = true;
                                            verdict = v;
                                        }
                                        policy::Verdict::Blocked { reason }
                                        | policy::Verdict::Uncertain { reason, .. } => {
                                            note.note = format!("{} — {reason}", note.note);
                                        }
                                        policy::Verdict::NeedsText => {}
                                    }
                                } else if needs_text && dictated.is_some() {
                                    // Text arrived: the original decision can now go ahead.
                                    verdict = policy::judge(
                                        &signals,
                                        &reduced,
                                        dictated.as_deref(),
                                        allow_irreversible,
                                    );
                                    note.applied = matches!(verdict, policy::Verdict::Act { .. });
                                    two_decided = note.applied;
                                }
                                two_notes.push(note);
                            }
                            None if two_outcome.is_none() => two_notes.push(uc_two::Note {
                                kind: uc_two::Kind::Rescue,
                                model: two.model().to_string(),
                                ms: ms(t_wait),
                                cost_usd: 0.0,
                                note: "no reply within the wait budget".into(),
                                applied: false,
                            }),
                            None => {}
                        }
                    }
                }
            }

            let mut rec = StepRecord {
                step: steps,
                ts_unix: ts_unix(),
                app: scene.app.clone(),
                title: scene.title.clone(),
                raw_elements: scan.raw_count,
                popups,
                subgoal: subgoal_label.clone(),
                two: two_notes,
                sent_elements: reduced.len(),
                est_tokens,
                tree_hash: hash,
                changed,
                scan_ms,
                jev_ms,
                act_ms: 0.0,
                total_ms: 0.0,
                input_tokens: step_tokens,
                cost_usd: step_cost,
                hedged_winner: decision.timing.winner,
                narrowed,
                signals_first,
                signals,
                verdict: verdict.clone(),
                executed: false,
            };

            // 5. Act (or not).
            let mut next: Option<Outcome> = two_outcome;
            match &verdict {
                _ if next.is_some() => {}
                policy::Verdict::Done => match &plan {
                    Some(p) if plan_i + 1 < p.len() => {
                        // Sub-goal done: on to the next one, counters fresh.
                        last = Some(json!({
                            "op": "subgoal_done",
                            "subgoal": p[plan_i],
                            "effect": "moving on to the next sub-goal",
                        }));
                        plan_i += 1;
                        last_hash = None;
                        last_was_wait = false;
                        uncertain = 0;
                        entropy_strikes = 0;
                        stall = 0;
                        waits = 0;
                    }
                    _ => next = Some(Outcome::Done),
                },
                policy::Verdict::NeedsText => next = Some(Outcome::NeedsText),
                policy::Verdict::Blocked { reason } => {
                    next = Some(Outcome::Blocked(reason.clone()))
                }
                policy::Verdict::Uncertain { reason, .. } => {
                    uncertain += 1;
                    if uncertain >= consts::UNCERTAIN_STRIKES {
                        next = Some(Outcome::Uncertain(reason.clone()));
                    } else {
                        last =
                            Some(json!({"op": "look", "effect": format!("uncertain: {reason}")}));
                        last_hash = None;
                    }
                }
                policy::Verdict::Act { action } => {
                    uncertain = 0;
                    // The narrowed round reports the gates' entropy (0); the guard must
                    // look at the first, full distribution.
                    let entropy = rec
                        .signals_first
                        .as_ref()
                        .unwrap_or(&rec.signals)
                        .target_entropy;
                    // Jev's entropy says nothing about a step System Two chose.
                    if matches!(action, policy::Action::Click { .. }) && !two_decided {
                        if entropy > consts::ENTROPY_MAX {
                            entropy_strikes += 1;
                        } else {
                            entropy_strikes = 0;
                        }
                    }
                    if entropy_strikes >= consts::ENTROPY_STRIKES {
                        next = Some(Outcome::Uncertain(format!(
                            "target entropy {entropy:.2} for {entropy_strikes} steps"
                        )));
                    } else if !self.opts.act {
                        next = Some(Outcome::Preview);
                    } else if uc_win32::kill_switch_pressed() {
                        next = Some(Outcome::Killed);
                    } else if self.stop_requested() {
                        next = Some(Outcome::Stopped);
                    } else if !focus_still_ours(locked_pid, target_hwnd) {
                        // Deciding took 0.3–2 s; a toast, a UAC prompt or an Alt-Tab may
                        // have moved the foreground meanwhile. Never inject blind.
                        next = Some(Outcome::FocusLost(
                            "foreground changed while deciding".into(),
                        ));
                    } else {
                        let t_act = Instant::now();
                        exec::perform(action)?;
                        rec.act_ms = ms(t_act);
                        rec.executed = true;
                        last = Some(action_summary(action));
                        last_hash = Some(hash);
                        last_was_wait = matches!(action, policy::Action::Wait);
                        if last_was_wait {
                            waits += 1;
                            if waits >= consts::WAIT_STRIKES {
                                next = Some(Outcome::Stalled);
                            }
                        } else {
                            waits = 0;
                        }
                        std::thread::sleep(Duration::from_millis(consts::SETTLE_CAP_MS));
                    }
                }
            }
            rec.total_ms = ms(t0);
            if let Some(hook) = self.on_step.as_mut() {
                hook(&rec);
            }
            if let Some((f, _)) = ledger.as_mut() {
                writeln!(f, "{}", serde_json::to_string(&rec)?)?;
            }
            if let Some(o) = next {
                break o;
            }
        };

        let summary = RunSummary {
            goal: goal.to_string(),
            outcome,
            steps,
            elapsed_ms: ms(t_run),
            jev_calls,
            cost_usd: cost,
            ledger: ledger.as_ref().map(|(_, p)| p.clone()),
            two_model: self.two.as_ref().map(|t| t.model().to_string()),
            two_calls: self.two.as_ref().map_or(0, |t| t.calls()),
            two_cost_usd: self.two.as_ref().map_or(0.0, |t| t.cost_usd()),
            plan,
        };
        if let Some((f, _)) = ledger.as_mut() {
            writeln!(f, "{}", serde_json::to_string(&summary)?)?;
        }
        Ok(summary)
    }
}

/// Is the locked process still in front and the target window still alive? Checked
/// at the scan and again right before injecting.
fn focus_still_ours(locked_pid: Option<u32>, target_hwnd: Option<isize>) -> bool {
    let alive = target_hwnd
        .is_none_or(|h| uc_win32::is_window(uc_win32::HWND(h as *mut core::ffi::c_void)));
    let fg_pid = uc_win32::foreground_hwnd().map(uc_win32::window_pid);
    alive && fg_pid.is_some() && fg_pid == locked_pid
}

fn stop_set(flag: &Option<Arc<AtomicBool>>) -> bool {
    flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Fold one System Two reply into the run: a plan is adopted once (never replaced),
/// a rephrased sub-goal overrides the current one (only when `allow_subgoal`: a reply
/// answering an older step must not rewrite the current one), text fills an empty
/// dictation. A proposed step (`next`) is left to the caller, which has the elements
/// to vet it. Without a plan, a sub-goal becomes step one *before* the overall goal,
/// so finishing it can never end the run as "goal reached".
fn apply_reply(
    r: &uc_two::Reply,
    goal: &str,
    plan: &mut Option<Vec<String>>,
    plan_i: &mut usize,
    dictated: &mut Option<String>,
    allow_subgoal: bool,
) -> uc_two::Note {
    let mut note = uc_two::Note {
        kind: r.kind,
        model: r.model.clone(),
        ms: r.ms,
        cost_usd: r.cost_usd,
        note: String::new(),
        applied: false,
    };
    match &r.advice {
        Err(e) => note.note = format!("error: {e}"),
        Ok(a) => {
            note.note = a.note.clone().unwrap_or_default();
            if let (Some(steps), None) = (&a.plan, &*plan) {
                *plan = Some(steps.clone());
                *plan_i = 0;
                note.applied = true;
                note.note = format!(
                    "plan ({}): {} | {}",
                    steps.len(),
                    steps.join(" → "),
                    note.note
                );
            }
            if let (Some(sg), true) = (&a.subgoal, allow_subgoal) {
                match plan.as_mut() {
                    Some(p) => p[*plan_i] = sg.clone(),
                    None => *plan = Some(vec![sg.clone(), goal.to_string()]),
                }
                note.applied = true;
            }
            if let (Some(t), None) = (&a.text, &*dictated) {
                *dictated = Some(t.clone());
                note.applied = true;
            }
        }
    }
    note
}

fn open_ledger(dir: &Path, goal: &str) -> Result<(std::fs::File, PathBuf), LoopError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.jsonl", ts_unix(), slug(goal)));
    Ok((std::fs::File::create(&path)?, path))
}

/// What the next state says about the previous action (`last` in the Jev state).
fn action_summary(a: &policy::Action) -> Value {
    match a {
        policy::Action::Click { name, right, .. } => {
            json!({"op": if *right { "right_click" } else { "click" }, "target": name})
        }
        policy::Action::Type {
            text, target_name, ..
        } => json!({"op": "type", "text": text, "target": target_name}),
        policy::Action::Key { key } => json!({"op": "key", "key": key}),
        policy::Action::Scroll { notches, .. } => {
            json!({"op": if *notches > 0 { "scroll_down" } else { "scroll_up" }})
        }
        policy::Action::Wait => json!({"op": "wait"}),
    }
}

#[cfg(test)]
mod tests {
    use super::policy::{judge, Action, Signals, Verdict};
    use super::*;
    use uc_uia::Element;

    fn el(i: usize, role: &str, name: &str) -> Element {
        Element {
            i,
            role: role.into(),
            name: name.into(),
            bbox: [100 * i as i32, 50, 80, 30],
            enabled: true,
            val: None,
            focused: false,
            auto_id: None,
        }
    }

    fn sig(op: &str, target: &str, conf: f64, gap: f64) -> Signals {
        Signals {
            target: target.into(),
            target_conf: conf,
            target_gap: gap,
            target_entropy: 0.1,
            op: op.into(),
            op_conf: 0.9,
            key: "none".into(),
            key_conf: 0.9,
            ..Default::default()
        }
    }

    #[test]
    fn done_needs_both_signals() {
        let els = [el(0, "button", "OK")];
        let mut s = sig("done", "none", 0.9, 0.9);
        s.goal_reached = 0.95;
        assert_eq!(judge(&s, &els, None, false), Verdict::Done);
        s.goal_reached = 0.4;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
        let mut s = sig("click", "e0", 0.9, 0.9);
        s.goal_reached = 0.95;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
        let mut s = sig("done", "none", 0.9, 0.9);
        s.goal_reached = 0.95;
        s.op_conf = 0.35;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
    }

    #[test]
    fn disabled_target_is_never_acted_on() {
        let mut els = [el(0, "button", "Zapisz")];
        els[0].enabled = false;
        assert!(matches!(
            judge(&sig("click", "e0", 0.95, 0.9), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        match judge(&sig("type", "e0", 0.95, 0.9), &els, Some("x"), false) {
            Verdict::Act {
                action: Action::Type { focus, .. },
            } => assert_eq!(focus, None),
            other => panic!("expected type without focus, got {other:?}"),
        }
    }

    #[test]
    fn destructive_typing_needs_the_irreversible_bar() {
        let els = [el(0, "edit", "Name")];
        let mut s = sig("type", "e0", 0.9, 0.9);
        s.is_destructive = 0.7;
        assert!(matches!(
            judge(&s, &els, Some("x"), false),
            Verdict::Blocked { .. }
        ));
        s.op_conf = 0.7;
        assert!(matches!(
            judge(&s, &els, Some("x"), true),
            Verdict::Uncertain { .. }
        ));
        s.op_conf = 0.9;
        assert!(matches!(
            judge(&s, &els, Some("x"), true),
            Verdict::Act { .. }
        ));
    }

    #[test]
    fn click_gates_on_confidence_gap_and_irreversibility() {
        let els = [el(0, "button", "Save"), el(1, "button", "Delete")];
        match judge(&sig("click", "e0", 0.93, 0.88), &els, None, false) {
            Verdict::Act {
                action: Action::Click { target, x, y, .. },
            } => {
                assert_eq!(target, 0);
                assert_eq!((x, y), (40, 65));
            }
            other => panic!("expected click, got {other:?}"),
        }
        assert!(matches!(
            judge(&sig("click", "e0", 0.55, 0.5), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "e0", 0.9, 0.05), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "none", 0.9, 0.9), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        // "Delete" is on the list: blocked without the flag, needs 0.85 with it.
        assert!(matches!(
            judge(&sig("click", "e1", 0.95, 0.9), &els, None, false),
            Verdict::Blocked { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "e1", 0.95, 0.9), &els, None, true),
            Verdict::Act { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "e1", 0.7, 0.5), &els, None, true),
            Verdict::Uncertain { .. }
        ));
        // Jev's own destructive signal blocks even a harmless-looking name.
        let mut s = sig("click", "e0", 0.95, 0.9);
        s.is_destructive = 0.7;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Blocked { .. }
        ));
    }

    #[test]
    fn close_call_between_two_candidates_asks_to_narrow() {
        let els = [
            el(0, "button", "Zamknij"),
            el(1, "menuitem", "Plik"),
            el(2, "button", "OK"),
        ];
        let mut s = sig("click", "e0", 0.47, 0.08);
        s.target_top = vec![
            ("e0".into(), 0.47),
            ("e1".into(), 0.39),
            ("e2".into(), 0.13),
        ];
        match judge(&s, &els, None, false) {
            Verdict::Uncertain { narrow, .. } => {
                assert_eq!(narrow, Some(vec!["e0".to_string(), "e1".to_string()]))
            }
            other => panic!("expected uncertain+narrow, got {other:?}"),
        }
        // `none` as runner-up is not a shortlist.
        s.target_top = vec![
            ("e0".into(), 0.47),
            ("none".into(), 0.39),
            ("e2".into(), 0.13),
        ];
        match judge(&s, &els, None, false) {
            Verdict::Uncertain { narrow, .. } => assert_eq!(narrow, None),
            other => panic!("expected uncertain without narrow, got {other:?}"),
        }
    }

    #[test]
    fn type_needs_dictated_text_and_focuses_the_target() {
        let els = [el(0, "edit", "Text editor")];
        assert_eq!(
            judge(&sig("type", "e0", 0.9, 0.9), &els, None, false),
            Verdict::NeedsText
        );
        match judge(&sig("type", "e0", 0.9, 0.9), &els, Some("hello"), false) {
            Verdict::Act {
                action: Action::Type { text, focus, .. },
            } => {
                assert_eq!(text, "hello");
                assert_eq!(focus, Some((40, 65)));
            }
            other => panic!("expected type, got {other:?}"),
        }
        match judge(&sig("type", "none", 0.9, 0.9), &els, Some("hello"), false) {
            Verdict::Act {
                action: Action::Type { focus, .. },
            } => assert_eq!(focus, None),
            other => panic!("expected type, got {other:?}"),
        }
    }

    #[test]
    fn right_click_and_destructive_keys() {
        let els = [el(0, "listitem", "Pulpit")];
        match judge(&sig("right_click", "e0", 0.9, 0.9), &els, None, false) {
            Verdict::Act {
                action: Action::Click { right, .. },
            } => assert!(right),
            other => panic!("expected right click, got {other:?}"),
        }
        let mut s = sig("key", "none", 0.9, 0.9);
        s.key = "delete".into();
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Blocked { .. }
        ));
        s.key_conf = 0.95;
        assert!(matches!(judge(&s, &els, None, true), Verdict::Act { .. }));
    }

    #[test]
    fn advice_goes_through_the_gates() {
        use uc_two::Next;
        let els = [el(0, "button", "Save"), el(1, "button", "Delete")];
        let next = |op: &str, target: Option<&str>| Next {
            op: op.into(),
            target: target.map(str::to_string),
            key: None,
            text: None,
        };
        let judge =
            |n: &Next, allow: bool, destr: f64| policy::from_advice(n, &els, None, allow, destr);
        assert!(matches!(
            judge(&next("click", Some("e0")), false, 0.0),
            Verdict::Act {
                action: Action::Click { target: 0, .. }
            }
        ));
        // Jev's own destructive reading gates the LLM's click on a harmless-looking name
        assert!(matches!(
            judge(&next("click", Some("e0")), false, 0.9),
            Verdict::Blocked { .. }
        ));
        assert!(matches!(
            judge(&next("click", Some("e1")), false, 0.0),
            Verdict::Blocked { .. }
        ));
        assert!(matches!(
            judge(&next("click", Some("e1")), true, 0.0),
            Verdict::Act { .. }
        ));
        assert!(matches!(
            judge(&next("click", Some("e7")), false, 0.0),
            Verdict::Uncertain { .. }
        ));
        assert!(matches!(
            judge(&next("type", None), false, 0.0),
            Verdict::NeedsText
        ));
        let mut typed = next("type", Some("e0"));
        typed.text = Some("hi".into());
        assert!(matches!(
            judge(&typed, false, 0.0),
            Verdict::Act {
                action: Action::Type { .. }
            }
        ));
        let mut del = next("key", None);
        del.key = Some("delete".into());
        assert!(matches!(judge(&del, false, 0.0), Verdict::Blocked { .. }));
        let mut odd = next("key", None);
        odd.key = Some("ctrl+alt+del".into());
        assert!(matches!(judge(&odd, true, 0.0), Verdict::Uncertain { .. }));
        assert!(matches!(
            judge(&next("done", None), false, 0.0),
            Verdict::Done
        ));
    }

    #[test]
    fn replies_fold_into_plan_text_and_subgoal() {
        let reply = |advice: uc_two::Advice| uc_two::Reply {
            kind: uc_two::Kind::Plan,
            tag: 1,
            advice: Ok(advice),
            model: "m".into(),
            ms: 1.0,
            cost_usd: 0.0,
            prompt_tokens: 0,
            completion_tokens: 0,
        };
        let mut plan = None;
        let mut i = 0usize;
        let mut dictated = None;
        let n = apply_reply(
            &reply(uc_two::Advice {
                plan: Some(vec!["a".into(), "b".into()]),
                text: Some("t".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            true,
        );
        assert!(n.applied);
        assert_eq!(
            plan.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
        assert_eq!(dictated.as_deref(), Some("t"));
        // a second plan never replaces the first; a sub-goal rewrites the current one
        i = 1;
        let n = apply_reply(
            &reply(uc_two::Advice {
                plan: Some(vec!["x".into()]),
                subgoal: Some("b2".into()),
                text: Some("ignored".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            true,
        );
        assert!(n.applied);
        assert_eq!(plan.as_ref().unwrap()[1], "b2");
        assert_eq!(dictated.as_deref(), Some("t"));
        // a late reply may not rewrite the current sub-goal
        let n = apply_reply(
            &reply(uc_two::Advice {
                subgoal: Some("late".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            false,
        );
        assert!(!n.applied);
        assert_eq!(plan.as_ref().unwrap()[1], "b2");
        let n = apply_reply(
            &uc_two::Reply {
                advice: Err("boom".into()),
                ..reply(uc_two::Advice::default())
            },
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            true,
        );
        assert!(!n.applied);
        assert!(n.note.starts_with("error"));
        // without a plan, a sub-goal is step one and the overall goal stays last
        let mut plan2 = None;
        let mut i2 = 0usize;
        let mut d2 = None;
        apply_reply(
            &reply(uc_two::Advice {
                subgoal: Some("open the menu".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan2,
            &mut i2,
            &mut d2,
            true,
        );
        assert_eq!(
            plan2.as_deref(),
            Some(&["open the menu".to_string(), "overall".to_string()][..])
        );
    }

    #[test]
    fn empty_spot_prefers_centre_then_grid_then_gives_up() {
        let rect = [0, 0, 1000, 800];
        assert_eq!(empty_spot(rect, &[]), Some((500, 416)));
        let mut mid = el(0, "button", "Mid");
        mid.bbox = [400, 350, 200, 150];
        assert!(
            matches!(empty_spot(rect, &[mid.clone()]), Some((x, y)) if !(388..612).contains(&x) || !(338..512).contains(&y))
        );
        let mut all = el(1, "document", "Doc");
        all.bbox = [-20, -20, 1040, 840];
        assert_eq!(empty_spot(rect, &[all]), None);
        assert_eq!(empty_spot([0, 0, 20, 20], &[]), None);
    }

    #[test]
    fn key_and_scroll() {
        let els = [el(0, "list", "Files")];
        let mut s = sig("key", "none", 0.9, 0.9);
        s.key = "ctrl+s".into();
        assert_eq!(
            judge(&s, &els, None, false),
            Verdict::Act {
                action: Action::Key {
                    key: "ctrl+s".into()
                }
            }
        );
        s.key = "none".into();
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
        assert_eq!(
            judge(&sig("scroll_down", "e0", 0.9, 0.9), &els, None, false),
            Verdict::Act {
                action: Action::Scroll {
                    notches: consts::SCROLL_NOTCHES,
                    at: Some((40, 65))
                }
            }
        );
    }

    #[test]
    fn quoted_text_extraction() {
        assert_eq!(
            extract_quoted("wpisz „hello world” w edytorze"),
            Some("hello world".into())
        );
        assert_eq!(extract_quoted("type \"abc\" then save"), Some("abc".into()));
        assert_eq!(extract_quoted("type 'x y' now"), Some("x y".into()));
        assert_eq!(extract_quoted("no quotes here"), None);
        assert_eq!(extract_quoted("empty \"\" quotes"), None);
        assert_eq!(extract_quoted("Don't save and don't close"), None);
        assert_eq!(extract_quoted("it's 'ok' now"), Some("ok".into()));
    }

    #[test]
    fn bundle_has_seven_questions_and_compiles() {
        let els = [el(0, "button", "Save"), el(1, "edit", "Name")];
        let crit = questions::criteria(&els);
        assert_eq!(crit.len(), 3);
        assert_eq!(crit[0].0, "e0");
        assert_eq!(crit[2].0, "none");
        let b = questions::bundle(&crit);
        let mut names: Vec<&String> = b.keys().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "goal_pending",
                "goal_reached",
                "is_destructive",
                "key",
                "needs_text",
                "op",
                "target"
            ]
        );
        let c = questions::compile(&els);
        assert_eq!(c.names.len(), 7);
    }

    #[test]
    fn slug_is_filename_safe() {
        assert_eq!(
            slug("Wpisz „hello” w Notatniku!"),
            "wpisz-hello-w-notatniku"
        );
    }
}
