//! Structural perception through UI Automation, on raw COM (`windows` crate).
//!
//! Performance rule (measured in the Python reference, `D:\code\JevUse`): every UIA
//! property read is a cross-process RPC. So the scan is ONE
//! `FindAllBuildCache(TreeScope_Descendants, OR(interactive control types), cache)` and
//! every property we need comes back in the cache — no per-element round trips. The
//! remaining cost (96–97 %) is inside the target application's accessibility provider
//! and is the same from any language.
//!
//! COM is apartment-bound: create and use a [`UiaScanner`] on ONE thread (STA).

use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::time::Instant;

use serde::Serialize;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BSTR, VT_I4,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
    IUIAutomationElement, TreeScope_Descendants, TreeScope_Element, UIA_AutomationIdPropertyId,
    UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId, UIA_HasKeyboardFocusPropertyId,
    UIA_IsEnabledPropertyId, UIA_IsOffscreenPropertyId, UIA_NamePropertyId,
    UIA_ValueValuePropertyId, UIA_CONTROLTYPE_ID,
};

/// Canonical element record — identical shape to the Python reference (`guistate.py`) so
/// measurements and question bundles transfer 1:1. `box` is `[x, y, w, h]` physical px.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Element {
    pub i: usize,
    pub role: String,
    pub name: String,
    #[serde(rename = "box")]
    pub bbox: [i32; 4],
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub val: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub focused: bool,
    #[serde(skip)]
    pub auto_id: Option<String>,
}

fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl Element {
    pub fn center(&self) -> (i32, i32) {
        let [x, y, w, h] = self.bbox;
        (x + w / 2, y + h / 2)
    }
}

/// Result of one scan with stage timing.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Scan {
    pub elements: Vec<Element>,
    /// Elements returned by the provider before our filters (offscreen, empty box).
    pub raw_count: usize,
    pub find_ms: f64,
    pub read_ms: f64,
    pub total_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// UIA ControlType ids (UIA_*ControlTypeId, 50000..50040).
const CT_NAMES: [(i32, &str); 41] = [
    (50000, "button"),
    (50001, "calendar"),
    (50002, "checkbox"),
    (50003, "combobox"),
    (50004, "edit"),
    (50005, "link"),
    (50006, "image"),
    (50007, "listitem"),
    (50008, "list"),
    (50009, "menu"),
    (50010, "menubar"),
    (50011, "menuitem"),
    (50012, "progressbar"),
    (50013, "radio"),
    (50014, "scrollbar"),
    (50015, "slider"),
    (50016, "spinner"),
    (50017, "statusbar"),
    (50018, "tab"),
    (50019, "tabitem"),
    (50020, "text"),
    (50021, "toolbar"),
    (50022, "tooltip"),
    (50023, "tree"),
    (50024, "treeitem"),
    (50025, "custom"),
    (50026, "group"),
    (50027, "thumb"),
    (50028, "datagrid"),
    (50029, "dataitem"),
    (50030, "document"),
    (50031, "splitbutton"),
    (50032, "window"),
    (50033, "pane"),
    (50034, "header"),
    (50035, "headeritem"),
    (50036, "table"),
    (50037, "titlebar"),
    (50038, "separator"),
    (50039, "semanticzoom"),
    (50040, "appbar"),
];

/// Control types that can be acted on. Mirrors `CORE_INTERACTIVE` in the Python reference.
pub const INTERACTIVE_IDS: [i32; 15] = [
    50000, 50002, 50003, 50004, 50005, 50007, 50011, 50013, 50015, 50016, 50019, 50024, 50029,
    50031, 50035,
];

/// Extra types scanned only when `include_context` is on (labels, custom widgets):
/// they give Jev context ("Payment successful") at the cost of state size.
pub const CONTEXT_IDS: [i32; 3] = [50020, 50025, 50030];

pub fn role_name(ct: i32) -> &'static str {
    CT_NAMES
        .iter()
        .find(|(id, _)| *id == ct)
        .map(|(_, n)| *n)
        .unwrap_or("custom")
}

/// `VT_I4` VARIANT built by hand — `windows` 0.62 ships no `From<i32>` for it.
fn variant_i32(v: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: v },
            }),
        },
    }
}

/// String payload of a `VT_BSTR` VARIANT, `None` for any other type.
unsafe fn variant_string(v: &VARIANT) -> Option<String> {
    let inner = &*v.Anonymous.Anonymous;
    if inner.vt == VT_BSTR {
        let s = (*inner.Anonymous.bstrVal).to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    }
}

pub struct UiaScanner {
    uia: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    cond_interactive: IUIAutomationCondition,
    cond_with_context: IUIAutomationCondition,
}

impl UiaScanner {
    /// Initialises COM (STA) on the calling thread and builds the cache request and
    /// conditions once. Everything afterwards is one RPC batch per scan.
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: COM initialisation on this thread; S_FALSE / RPC_E_CHANGED_MODE are
        // benign (already initialised) and ignored on purpose.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            let cache = uia.CreateCacheRequest()?;
            for p in [
                UIA_NamePropertyId,
                UIA_AutomationIdPropertyId,
                UIA_ControlTypePropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_IsEnabledPropertyId,
                UIA_IsOffscreenPropertyId,
                UIA_ValueValuePropertyId,
                UIA_HasKeyboardFocusPropertyId,
            ] {
                cache.AddProperty(p)?;
            }
            cache.SetTreeScope(TreeScope_Element)?;
            let cond_interactive = Self::or_condition(&uia, &INTERACTIVE_IDS)?;
            let all: Vec<i32> = INTERACTIVE_IDS
                .iter()
                .chain(CONTEXT_IDS.iter())
                .copied()
                .collect();
            let cond_with_context = Self::or_condition(&uia, &all)?;
            Ok(Self {
                uia,
                cache,
                cond_interactive,
                cond_with_context,
            })
        }
    }

    unsafe fn or_condition(
        uia: &IUIAutomation,
        ids: &[i32],
    ) -> windows::core::Result<IUIAutomationCondition> {
        let conds: Vec<Option<IUIAutomationCondition>> = ids
            .iter()
            .map(|id| {
                uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &variant_i32(*id))
                    .ok()
            })
            .collect();
        uia.CreateOrConditionFromNativeArray(&conds)
    }

    /// One batched scan of a window's descendants.
    pub fn scan_hwnd(&self, hwnd: HWND, include_context: bool) -> Scan {
        let t0 = Instant::now();
        let mut out = Scan::default();
        let cond = if include_context {
            &self.cond_with_context
        } else {
            &self.cond_interactive
        };
        // SAFETY: COM calls on the thread that created the scanner; every interface is
        // owned for the duration of the call.
        let res: windows::core::Result<()> = unsafe {
            (|| {
                let root = self.uia.ElementFromHandle(hwnd)?;
                let arr = root.FindAllBuildCache(TreeScope_Descendants, cond, &self.cache)?;
                out.find_ms = ms(t0);
                let t1 = Instant::now();
                let n = arr.Length()?.max(0) as usize;
                out.raw_count = n;
                let mut seen_focus = false;
                for idx in 0..n {
                    let Ok(e) = arr.GetElement(idx as i32) else {
                        continue;
                    };
                    if let Some(el) = read_cached(&e, out.elements.len(), &mut seen_focus) {
                        out.elements.push(el);
                    }
                }
                out.read_ms = ms(t1);
                Ok(())
            })()
        };
        if let Err(e) = res {
            out.error = Some(format!("{e}"));
        }
        out.total_ms = ms(t0);
        out
    }

    pub fn scan_foreground(&self, include_context: bool) -> Scan {
        match uc_win32::foreground_hwnd() {
            Some(h) => self.scan_hwnd(h, include_context),
            None => Scan {
                error: Some("no foreground window".into()),
                ..Default::default()
            },
        }
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Read one element entirely from the cache (zero RPC), except the text value of edit
/// fields, which some providers (WinForms) leave empty in the cache — one RPC per field.
unsafe fn read_cached(
    e: &IUIAutomationElement,
    i: usize,
    seen_focus: &mut bool,
) -> Option<Element> {
    if e.CachedIsOffscreen().ok()?.as_bool() {
        return None;
    }
    let r = e.CachedBoundingRectangle().ok()?;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let ct: UIA_CONTROLTYPE_ID = e.CachedControlType().ok()?;
    let role = role_name(ct.0).to_string();
    let name = e.CachedName().map(|b| b.to_string()).unwrap_or_default();
    let auto_id = e
        .CachedAutomationId()
        .map(|b| b.to_string())
        .ok()
        .filter(|s| !s.is_empty());
    let enabled = e.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true);
    let focused = !*seen_focus
        && e.CachedHasKeyboardFocus()
            .map(|b| b.as_bool())
            .unwrap_or(false);
    if focused {
        *seen_focus = true;
    }
    let mut val = None;
    if matches!(role.as_str(), "edit" | "combobox" | "spinner") {
        val = e
            .GetCachedPropertyValue(UIA_ValueValuePropertyId)
            .ok()
            .and_then(|v| variant_string(&v));
        if val.is_none() {
            // Cache empty (WinForms does this): one RPC for the live value.
            val = e
                .GetCurrentPropertyValue(UIA_ValueValuePropertyId)
                .ok()
                .and_then(|v| variant_string(&v));
        }
    }
    let name = if name.is_empty() {
        auto_id.clone().unwrap_or_default()
    } else {
        name
    };
    Some(Element {
        i,
        role,
        name,
        bbox: [r.left, r.top, w, h],
        enabled,
        val,
        focused,
        auto_id,
    })
}

// ---------------------------------------------------------------------------------------
// Reduction (port of the reference `reduce.py`) and hashing.
// ---------------------------------------------------------------------------------------

/// Form controls are meaningful even without a name (an empty text field).
const UNNAMED_OK: [&str; 7] = [
    "edit", "checkbox", "combobox", "radio", "slider", "spinner", "text",
];

fn role_weight(role: &str) -> i32 {
    match role {
        "button" | "splitbutton" | "link" | "menuitem" => 3,
        "tabitem" | "checkbox" | "combobox" | "edit" | "radio" | "slider" | "spinner" => 2,
        "listitem" | "treeitem" | "dataitem" | "headeritem" | "custom" | "text" => 1,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReduceOpts {
    pub max_n: usize,
    pub name_max: usize,
    pub val_max: usize,
    /// Prefer elements near this point (usually the cursor or focused control).
    pub near: Option<(i32, i32)>,
    /// Keep only elements intersecting this rect (usually the window rect).
    pub viewport: Option<[i32; 4]>,
}

impl Default for ReduceOpts {
    fn default() -> Self {
        Self {
            max_n: 60,
            name_max: 48,
            val_max: 24,
            near: None,
            viewport: None,
        }
    }
}

fn intersects(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Interactivity filter → viewport clip → dedupe (role, name) → priority sort → cap →
/// truncation → re-index `e0..eN`. Pure code, ≤ 2 ms for hundreds of elements.
pub fn reduce(elements: &[Element], opts: ReduceOpts) -> Vec<Element> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut kept: Vec<&Element> = Vec::with_capacity(elements.len());
    for el in elements {
        if el.name.is_empty() && !UNNAMED_OK.contains(&el.role.as_str()) {
            continue;
        }
        if el.role == "text" && el.name.chars().count() < 2 {
            continue;
        }
        if let Some(vp) = opts.viewport {
            if !intersects(el.bbox, vp) {
                continue;
            }
        }
        if !seen.insert((el.role.clone(), el.name.clone())) {
            continue;
        }
        kept.push(el);
    }
    kept.sort_by_key(|e| {
        let dist = opts
            .near
            .map(|(cx, cy)| {
                let (ex, ey) = e.center();
                (ex - cx).abs() + (ey - cy).abs()
            })
            .unwrap_or(0);
        (if e.focused { -1 } else { 0 }, -role_weight(&e.role), dist)
    });
    kept.truncate(opts.max_n);
    kept.into_iter()
        .enumerate()
        .map(|(i, e)| Element {
            i,
            role: e.role.clone(),
            name: truncate(&e.name, opts.name_max),
            bbox: e.bbox,
            enabled: e.enabled,
            val: e.val.as_deref().map(|v| truncate(v, opts.val_max)),
            focused: e.focused,
            auto_id: e.auto_id.clone(),
        })
        .collect()
}

/// Order-sensitive hash of the *normalised* tree (role, name, value, enabled — no
/// geometry). Used for "did the screen change" and the macro cache key. Comparing two
/// states is a job for code, not for Jev (measured: Jev answers 0.42–0.60 on obvious
/// changes).
pub fn tree_hash(elements: &[Element]) -> u64 {
    let mut buf = Vec::with_capacity(elements.len() * 24);
    for e in elements {
        buf.extend_from_slice(e.role.as_bytes());
        buf.push(0x1f);
        buf.extend_from_slice(e.name.as_bytes());
        buf.push(0x1f);
        if let Some(v) = &e.val {
            buf.extend_from_slice(v.as_bytes());
        }
        buf.push(if e.enabled { 1 } else { 0 });
        buf.push(0x1e);
    }
    xxhash_rust::xxh3::xxh3_64(&buf)
}

/// The text `state` sent to Jev for one step.
#[derive(Clone, Debug, Serialize)]
pub struct GuiState<'a> {
    pub goal: &'a str,
    pub scene: &'a uc_win32::Scene,
    pub elements: &'a [Element],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dictated: Option<&'a str>,
}

impl GuiState<'_> {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("GuiState serialises")
    }
    /// Cheap token estimate (bytes / 4) for budgeting; the provider reports the real count.
    pub fn estimate_tokens(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len() / 4).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(i: usize, role: &str, name: &str, x: i32) -> Element {
        Element {
            i,
            role: role.into(),
            name: name.into(),
            bbox: [x, 0, 10, 10],
            enabled: true,
            val: None,
            focused: false,
            auto_id: None,
        }
    }

    #[test]
    fn reduce_dedupes_sorts_and_reindexes() {
        let els = vec![
            el(0, "text", "Header", 0),
            el(1, "button", "Save", 100),
            el(2, "button", "Save", 200),
            el(3, "edit", "", 50),
        ];
        let r = reduce(
            &els,
            ReduceOpts {
                max_n: 10,
                ..Default::default()
            },
        );
        assert_eq!(
            r.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["Save", "", "Header"]
        );
        assert_eq!(r.iter().map(|e| e.i).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn hash_ignores_geometry_but_not_text() {
        let a = vec![el(0, "button", "Save", 0)];
        let mut b = a.clone();
        b[0].bbox = [500, 500, 10, 10];
        assert_eq!(tree_hash(&a), tree_hash(&b));
        b[0].name = "Saved".into();
        assert_ne!(tree_hash(&a), tree_hash(&b));
    }
}
