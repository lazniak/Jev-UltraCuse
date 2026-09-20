//! Step loop — constants first (the vendor's advice: "put the constants (questions and
//! thresholds) in a single place so they're easy to review"). The loop itself lands in
//! phase 1 (see TASKS.md); this file already fixes the decision contract.

pub mod consts {
    /// Universal floor from the confidence-routing pattern: below it, never act.
    pub const CONF_FLOOR: f64 = 0.60;
    /// System keys (Win, Alt+F4) and window-level operations.
    pub const CONF_SYSTEM_KEY: f64 = 0.75;
    /// Irreversible actions: threshold AND a spoken confirmation, always.
    pub const CONF_IRREVERSIBLE: f64 = 0.85;
    /// `goal_reached` must clear this AND pass a code-side check before the loop stops.
    pub const DONE_THRESHOLD: f64 = 0.85;
    /// Narrow (region → element) instead of accepting when the top-2 gap is below this.
    pub const TOP2_GAP_MIN: f64 = 0.15;
    /// Escalate after this many consecutive steps with normalised entropy above 0.6.
    pub const ENTROPY_MAX: f64 = 0.60;
    pub const ENTROPY_STRIKES: u32 = 2;
    /// Max candidates sent to Jev (measured stable to 60).
    pub const MAX_CANDIDATES: usize = 60;
    /// Settle cap after an action (jev-ultrafast: 50 ms / 2 frames; combobox 200 ms).
    pub const SETTLE_CAP_MS: u64 = 200;
    /// Hedge an in-flight decision after this delay.
    pub const HEDGE_AFTER_MS: u64 = 400;
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

    pub fn is_irreversible_name(name: &str) -> bool {
        let n = name.to_lowercase();
        IRREVERSIBLE_MARKERS.iter().any(|m| n.contains(m))
    }
}
