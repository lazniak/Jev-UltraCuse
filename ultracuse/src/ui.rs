//! The window: a goal in, steps out. One eframe/egui viewport (wgpu, AccessKit for
//! screen readers), the loop on its own thread, messages over a channel.
//!
//! The window never becomes the target: the user picks a window (or "the last active
//! one"), the loop brings it to the front and locks onto its process. The loop itself is
//! untouched — this is the same `uc_loop::Runner` the CLI uses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;
use serde::{Deserialize, Serialize};
use uc_loop::policy::{Action, Verdict};
use uc_loop::{Outcome, RunOpts, RunSummary, Runner, StepRecord};
use uc_two::ModelInfo;
use uc_win32::WindowInfo;

const WINDOW_TITLE: &str = "Jev-UltraCuse";
const POLL_MS: u64 = 250;
/// Settings live next to the exe (portable); never the API keys — those stay in env.
const SETTINGS_FILE: &str = "ultracuse.settings.json";
const MODEL_LIST_MAX: usize = 60;
const TWO_COLOR: egui::Color32 = egui::Color32::from_rgb(200, 160, 255);
const WIDEN_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 200, 200);

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    provider: String,
    max_steps: usize,
    two_enabled: bool,
    two_model: String,
    two_favorites: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: "auto".into(),
            max_steps: uc_loop::consts::MAX_STEPS_DEFAULT,
            two_enabled: false,
            two_model: uc_loop::consts::TWO_DEFAULT_MODEL.into(),
            two_favorites: vec![
                uc_loop::consts::TWO_DEFAULT_MODEL.into(),
                "openai/gpt-4.1-mini".into(),
                "anthropic/claude-haiku-4.5".into(),
            ],
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(SETTINGS_FILE)))
            .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
    }
    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), t);
        }
    }
}

pub fn run_ui() -> Result<()> {
    uc_win32::hide_own_console();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([620.0, 720.0])
            .with_min_inner_size([480.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("okno: {e}"))
}

enum Msg {
    Ready {
        provider: String,
        warm_ms: f64,
        two: Option<String>,
    },
    Step(Box<StepRecord>),
    Done(Box<RunSummary>),
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Starting,
    Running,
    Finished,
}

#[derive(Clone, Copy, PartialEq)]
enum ProviderChoice {
    Auto,
    Typesafe,
    OpenRouter,
}

struct App {
    goal: String,
    text: String,
    act: bool,
    allow_irreversible: bool,
    max_steps: usize,
    provider: ProviderChoice,
    /// The last foreground window that is not ours: where the run starts (ADR-004 —
    /// nothing to pick; from there the loop finds its way, or the desktop).
    last_active: Option<WindowInfo>,
    state: State,
    status: String,
    steps: Vec<StepRecord>,
    summary: Option<RunSummary>,
    error: Option<String>,
    rx: Option<Receiver<Msg>>,
    stop: Arc<AtomicBool>,
    last_poll: Instant,
    show_settings: bool,
    own_pid: u32,
    settings: Settings,
    /// OpenRouter catalogue for the model picker (fetched on demand, in a thread).
    models: Option<Vec<ModelInfo>>,
    models_rx: Option<Receiver<Result<Vec<ModelInfo>, String>>>,
    models_err: Option<String>,
    model_filter: String,
    two_key: Option<&'static str>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.all_styles_mut(|s| {
            for font in s.text_styles.values_mut() {
                font.size *= 1.15;
            }
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        });
        let settings = Settings::load();
        let provider = match settings.provider.as_str() {
            "typesafe" => ProviderChoice::Typesafe,
            "openrouter" => ProviderChoice::OpenRouter,
            _ => ProviderChoice::Auto,
        };
        Self {
            goal: String::new(),
            text: String::new(),
            act: false,
            allow_irreversible: false,
            max_steps: settings.max_steps.clamp(1, 40),
            provider,
            last_active: None,
            state: State::Idle,
            status: "gotowe".into(),
            steps: Vec::new(),
            summary: None,
            error: None,
            rx: None,
            stop: Arc::new(AtomicBool::new(false)),
            last_poll: Instant::now(),
            show_settings: false,
            own_pid: std::process::id(),
            settings,
            models: None,
            models_rx: None,
            models_err: None,
            model_filter: String::new(),
            two_key: uc_two::Config::key_available(),
        }
    }

    fn save_settings(&mut self) {
        self.settings.provider = match self.provider {
            ProviderChoice::Auto => "auto",
            ProviderChoice::Typesafe => "typesafe",
            ProviderChoice::OpenRouter => "openrouter",
        }
        .into();
        self.settings.max_steps = self.max_steps;
        self.settings.save();
    }

    /// Fetch OpenRouter's catalogue once, off the UI thread.
    fn fetch_models(&mut self) {
        if self.models_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.models_rx = Some(rx);
        self.models_err = None;
        std::thread::spawn(move || {
            let key = uc_two::Config::key_available().and_then(uc_jev::user_env);
            let _ = tx.send(uc_two::list_models(key.as_deref()).map_err(|e| e.to_string()));
        });
    }

    fn poll_models(&mut self) {
        let Some(rx) = &self.models_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(list)) => {
                self.models = Some(list);
                self.models_rx = None;
            }
            Ok(Err(e)) => {
                self.models_err = Some(e);
                self.models_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.models_err = Some("wątek listy modeli zakończył się bez wyniku".into());
                self.models_rx = None;
            }
        }
    }

    fn model_price(&self, id: &str) -> Option<String> {
        let m = self.models.as_ref()?.iter().find(|m| m.id == id)?;
        Some(if m.is_free() {
            "darmowy".into()
        } else {
            format!(
                "${:.2} / ${:.2} za Mtok · {}k ctx",
                m.prompt_usd_per_mtok,
                m.completion_usd_per_mtok,
                m.context_length / 1000
            )
        })
    }

    fn running(&self) -> bool {
        matches!(self.state, State::Starting | State::Running)
    }

    fn can_start(&self) -> bool {
        !self.running() && !self.goal.trim().is_empty()
    }

    /// Where the run starts: the window the user was in before coming here, or the
    /// desktop when there is none.
    fn start_window(&self) -> Option<WindowInfo> {
        self.last_active.clone().or_else(|| {
            uc_win32::desktop_hwnd().map(|h| WindowInfo {
                hwnd: h.0 as isize,
                title: "Program Manager".into(),
                exe: "explorer.exe".into(),
                pid: uc_win32::window_pid(h),
                minimized: false,
            })
        })
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(win) = self.start_window() else {
            self.error =
                Some("Nie widzę żadnego okna ani pulpitu, od którego można zacząć.".into());
            return;
        };
        let goal = self.goal.trim().to_string();
        let dictated = if self.text.trim().is_empty() {
            uc_loop::extract_quoted(&goal)
        } else {
            Some(self.text.clone())
        };
        let provider = match self.provider {
            ProviderChoice::Auto => None,
            ProviderChoice::Typesafe => Some(uc_jev::Provider::Typesafe),
            ProviderChoice::OpenRouter => Some(uc_jev::Provider::OpenRouter),
        };
        let two = if self.settings.two_enabled {
            match uc_two::Config::discover(
                &self.settings.two_model,
                uc_loop::consts::TWO_MAX_CALLS,
                Duration::from_millis(uc_loop::consts::TWO_TIMEOUT_MS),
            ) {
                Ok(c) => Some(c),
                Err(e) => {
                    self.error = Some(format!("System Two: {e}"));
                    return;
                }
            }
        } else {
            None
        };
        let opts = RunOpts {
            act: self.act,
            max_steps: self.max_steps,
            allow_irreversible: self.allow_irreversible,
            dictated,
            ledger_dir: Some("runs".into()),
            provider,
            start_hwnd: Some(win.hwnd),
            stop: Some(self.stop.clone()),
            two,
        };
        self.stop.store(false, Ordering::Relaxed);
        self.steps.clear();
        self.summary = None;
        self.error = None;
        self.state = State::Starting;
        self.status = format!("start: {}", window_label(&win, 40));
        // Out of the way: the run works where the user was, not here.
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        let (tx, rx) = mpsc::channel::<Msg>();
        self.rx = Some(rx);
        let ctx = ctx.clone();
        let hwnd = win.hwnd;
        let to_desktop = win.is_desktop();
        let spawned = std::thread::Builder::new()
            .name("uc-run".into())
            .spawn(move || {
                let send = |m: Msg| {
                    let _ = tx.send(m);
                    ctx.request_repaint();
                };
                let mut runner = match Runner::new(opts) {
                    Ok(r) => r,
                    Err(e) => return send(Msg::Error(e.to_string())),
                };
                let warm_ms = match runner.warm() {
                    Ok(ms) => ms,
                    Err(e) => return send(Msg::Error(format!("rozgrzewka Jev: {e}"))),
                };
                send(Msg::Ready {
                    provider: format!("{:?}", runner.provider()),
                    warm_ms,
                    two: runner.two_model().map(str::to_string),
                });
                let tx_step = tx.clone();
                let ctx_step = ctx.clone();
                runner.on_step = Some(Box::new(move |r| {
                    let _ = tx_step.send(Msg::Step(Box::new(r.clone())));
                    ctx_step.request_repaint();
                }));
                std::thread::sleep(Duration::from_millis(250));
                if to_desktop {
                    uc_win32::show_desktop(uc_loop::consts::SHOW_DESKTOP_MAX);
                } else if !uc_win32::bring_to_front(uc_win32::HWND(hwnd as *mut core::ffi::c_void))
                {
                    return send(Msg::Error(
                        "nie udało się wysunąć okna startowego na wierzch".into(),
                    ));
                }
                match runner.run(&goal) {
                    Ok(s) => send(Msg::Done(Box::new(s))),
                    Err(e) => send(Msg::Error(e.to_string())),
                }
            });
        if let Err(e) = spawned {
            self.error = Some(format!("wątek: {e}"));
            self.state = State::Finished;
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let mut inbox = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(m) => inbox.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for m in inbox {
            self.handle(m, ctx);
        }
        if disconnected {
            self.rx = None;
            if self.running() {
                self.state = State::Finished;
                self.error
                    .get_or_insert_with(|| "wątek pętli zakończył się bez wyniku".into());
            }
        }
        if self.last_poll.elapsed() >= Duration::from_millis(POLL_MS) {
            self.last_poll = Instant::now();
            if !self.running() {
                self.track_last_active();
            }
        }
    }

    /// Remember the last foreground window that is not ours — the run starts there.
    fn track_last_active(&mut self) {
        let Some(h) = uc_win32::foreground_hwnd() else {
            return;
        };
        let pid = uc_win32::window_pid(h);
        if pid == 0 || pid == self.own_pid {
            return;
        }
        let title = uc_win32::window_title(h);
        if title.is_empty() {
            return;
        }
        self.last_active = Some(WindowInfo {
            hwnd: h.0 as isize,
            title,
            exe: uc_win32::process_exe(pid).unwrap_or_default(),
            pid,
            minimized: false,
        });
    }

    fn handle(&mut self, m: Msg, ctx: &egui::Context) {
        match m {
            Msg::Ready {
                provider,
                warm_ms,
                two,
            } => {
                self.state = State::Running;
                self.status = match two {
                    Some(m) => format!("{provider}, rozgrzewka {warm_ms:.0} ms · System Two: {m}"),
                    None => format!("{provider}, rozgrzewka {warm_ms:.0} ms — działa"),
                };
            }
            Msg::Step(r) => self.steps.push(*r),
            Msg::Done(s) => {
                self.status = outcome_text(&s.outcome);
                self.summary = Some(*s);
                self.state = State::Finished;
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            }
            Msg::Error(e) => {
                self.error = Some(e);
                self.state = State::Finished;
                self.status = "błąd".into();
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx_owned = root.ctx().clone();
        let ctx = &ctx_owned;
        self.poll(ctx);
        let ctrl_enter = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
        if ctrl_enter && self.can_start() {
            self.start(ctx);
        }
        if self.running() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.stop.store(true, Ordering::Relaxed);
        }

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(WINDOW_TITLE);
                ui.label(egui::RichText::new(&self.status).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Ustawienia").clicked() {
                        self.show_settings = true;
                    }
                    if self.running() {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("bottom").show(root, |ui| {
            ui.add_space(4.0);
            match &self.summary {
                Some(s) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(outcome_text(&s.outcome)).strong());
                        ui.label(format!(
                            "· {} kroków · {:.1} s · {} wywołań Jev · ${:.5}",
                            s.steps,
                            s.elapsed_ms / 1000.0,
                            s.jev_calls,
                            s.cost_usd
                        ));
                        if s.two_calls > 0 {
                            ui.colored_label(
                                TWO_COLOR,
                                format!(
                                    "· System Two {}: {} wywołań · ${:.4}",
                                    s.two_model.as_deref().unwrap_or("?"),
                                    s.two_calls,
                                    s.two_cost_usd
                                ),
                            );
                        }
                        if let Some(p) = &s.ledger {
                            ui.label(egui::RichText::new(format!("· {}", p.display())).weak());
                        }
                    });
                    if let Some(plan) = &s.plan {
                        ui.label(
                            egui::RichText::new(format!("plan: {}", plan.join(" → ")))
                                .color(TWO_COLOR)
                                .small(),
                        );
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "Ctrl+Enter = start · Esc = stop · Ctrl+Alt+K = kill-switch (zawsze, także poza oknem)",
                        )
                        .weak(),
                    );
                }
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            self.target_section(ui);
            ui.add_space(4.0);
            self.goal_section(ui);
            ui.add_space(4.0);
            self.controls(ui, ctx);
            ui.separator();
            if let Some(e) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(255, 110, 110), e);
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.steps.is_empty() && !self.running() {
                        ui.label(
                            egui::RichText::new(
                                "Kroki pojawią się tutaj: co pętla zobaczyła, co zdecydował Jev, co zrobiła.",
                            )
                            .weak(),
                        );
                    }
                    for rec in &self.steps {
                        step_card(ui, rec);
                    }
                });
        });

        if self.show_settings {
            self.settings_modal(ctx);
        }

        ctx.request_repaint_after(Duration::from_millis(if self.running() {
            100
        } else {
            POLL_MS
        }));
    }
}

impl App {
    fn target_section(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Start:").strong());
                match self.start_window() {
                    Some(w) => {
                        ui.label(egui::RichText::new(window_label(&w, 48)).color(WIDEN_COLOR));
                        ui.label(egui::RichText::new(format!("pid {}", w.pid)).weak().small());
                    }
                    None => {
                        ui.label(egui::RichText::new("brak okna — zacznę od pulpitu").weak());
                    }
                }
            });
            ui.label(
                egui::RichText::new(
                    "Nic nie wskazujesz: pętla rusza z okna, w którym byłeś przed chwilą, i sama                      szuka dalej — najpierw więcej elementów, potem przegląd otwartych okien                      (przełączy się albo zminimalizuje je do pulpitu), na końcu System Two.",
                )
                .weak()
                .small(),
            );
        });
    }

    fn goal_section(&mut self, ui: &mut egui::Ui) {
        let label = ui.label(egui::RichText::new("Cel").strong());
        ui.add(
            egui::TextEdit::multiline(&mut self.goal)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text(
                    "np. Wpisz „hello” w edytorze tekstu · Zamknij kartę bez zapisywania zmian",
                ),
        )
        .labelled_by(label.id);
        let label = ui.label("Tekst do wpisania (opcjonalnie — inaczej z cudzysłowu w celu)");
        ui.add(
            egui::TextEdit::singleline(&mut self.text)
                .desired_width(f32::INFINITY)
                .hint_text("Jev nigdy nie generuje tekstu; tekst pochodzi od Ciebie"),
        )
        .labelled_by(label.id);
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.act, "Uzbrojone (wstrzykuje mysz i klawiaturę)")
                .on_hover_text("Bez tego pętla pokazuje pierwszą decyzję i nic nie robi.");
            ui.checkbox(&mut self.allow_irreversible, "Zezwól na nieodwracalne")
                .on_hover_text("Usuń / wyślij / zapłać… i kroki ocenione jako destrukcyjne. Nadal wymagana pewność ≥ 0.85.");
            ui.label("Kroki");
            ui.add(egui::DragValue::new(&mut self.max_steps).range(1..=40));
        });
        ui.horizontal(|ui| {
            let start = ui.add_enabled(
                self.can_start(),
                egui::Button::new(egui::RichText::new("▶  Start").strong()),
            );
            if start.clicked() {
                self.start(ctx);
            }
            let stop = ui.add_enabled(self.running(), egui::Button::new("■  Stop"));
            if stop.clicked() {
                self.stop.store(true, Ordering::Relaxed);
            }
            if !self.act {
                ui.label(egui::RichText::new("tryb podglądu").weak());
            }
        });
    }

    fn settings_modal(&mut self, ctx: &egui::Context) {
        self.poll_models();
        if self.models.is_none() && self.models_err.is_none() {
            self.fetch_models();
        }
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading("Ustawienia");
            ui.separator();
            ui.label(egui::RichText::new("Końcówka Jev").strong());
            ui.radio_value(
                &mut self.provider,
                ProviderChoice::Auto,
                "auto (vendor, gdy jest JEV_API_KEY)",
            );
            ui.radio_value(
                &mut self.provider,
                ProviderChoice::Typesafe,
                "TypeSafe (api.typesafe.ai)",
            );
            ui.radio_value(
                &mut self.provider,
                ProviderChoice::OpenRouter,
                "OpenRouter (typesafe/jev-1.13)",
            );
            ui.separator();
            self.two_section(ui);
            ui.separator();
            ui.label(egui::RichText::new("Skróty").strong());
            ui.label("Ctrl+Enter start · Esc stop · Ctrl+Alt+K kill-switch");
            ui.add_space(6.0);
            if ui.button("Zamknij").clicked() {
                close = true;
            }
        });
        if close || modal.should_close() {
            self.show_settings = false;
            self.save_settings();
        }
    }

    /// System Two: on/off, the model (searchable catalogue + favourites), key status.
    fn two_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("System Two (LLM z OpenRouter, obok pętli Jev)").strong());
        ui.label(
            egui::RichText::new(
                "Plan podcelów na starcie i ratunek, gdy Jev utknie — nigdy nie blokuje pętli. \
                 Każda propozycja przechodzi te same bramki co decyzja Jev.",
            )
            .weak()
            .small(),
        );
        match self.two_key {
            Some(name) => {
                ui.label(
                    egui::RichText::new(format!("klucz: {name} ✓"))
                        .weak()
                        .small(),
                );
            }
            None => {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 90),
                    "brak klucza OpenRouter — ustaw OPENROUTER_API_KEY (zmienna użytkownika) i uruchom ponownie",
                );
            }
        }
        ui.checkbox(&mut self.settings.two_enabled, "włącz System Two");
        ui.horizontal_wrapped(|ui| {
            ui.label("Model:");
            ui.label(
                egui::RichText::new(&self.settings.two_model)
                    .strong()
                    .color(TWO_COLOR),
            );
            if let Some(p) = self.model_price(&self.settings.two_model) {
                ui.label(egui::RichText::new(p).weak().small());
            }
            let fav = self
                .settings
                .two_favorites
                .contains(&self.settings.two_model);
            if ui
                .small_button(if fav { "★" } else { "☆" })
                .on_hover_text("ulubiony")
                .clicked()
            {
                let id = self.settings.two_model.clone();
                if fav {
                    self.settings.two_favorites.retain(|f| f != &id);
                } else {
                    self.settings.two_favorites.push(id);
                }
            }
        });
        if !self.settings.two_favorites.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("ulubione:").weak().small());
                let favs = self.settings.two_favorites.clone();
                for f in favs {
                    if ui
                        .selectable_label(
                            self.settings.two_model == f,
                            egui::RichText::new(&f).small(),
                        )
                        .clicked()
                    {
                        self.settings.two_model = f;
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.model_filter)
                    .hint_text("szukaj: gemini, claude, gpt, :free …")
                    .desired_width(300.0),
            );
            if ui
                .button("↻")
                .on_hover_text("pobierz listę modeli ponownie")
                .clicked()
            {
                self.models = None;
                self.models_err = None;
                self.fetch_models();
            }
            if self.models_rx.is_some() {
                ui.spinner();
            }
        });
        if let Some(e) = &self.models_err {
            ui.colored_label(
                egui::Color32::from_rgb(255, 110, 110),
                format!("lista modeli: {e}"),
            );
        }
        let filter = self.model_filter.trim().to_ascii_lowercase();
        let mut pick: Option<String> = None;
        if let Some(models) = &self.models {
            let matching: Vec<&ModelInfo> = models
                .iter()
                .filter(|m| {
                    filter.is_empty()
                        || m.id.to_ascii_lowercase().contains(&filter)
                        || m.name.to_ascii_lowercase().contains(&filter)
                })
                .collect();
            ui.label(
                egui::RichText::new(format!(
                    "{} z {} modeli{}",
                    matching.len().min(MODEL_LIST_MAX),
                    models.len(),
                    if matching.len() > MODEL_LIST_MAX {
                        " (zawęź wyszukiwanie)"
                    } else {
                        ""
                    }
                ))
                .weak()
                .small(),
            );
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .show(ui, |ui| {
                    for m in matching.into_iter().take(MODEL_LIST_MAX) {
                        let label = if m.is_free() {
                            format!("{} · darmowy · {}k ctx", m.id, m.context_length / 1000)
                        } else {
                            format!(
                                "{} · ${:.2} / ${:.2} za Mtok · {}k ctx",
                                m.id,
                                m.prompt_usd_per_mtok,
                                m.completion_usd_per_mtok,
                                m.context_length / 1000
                            )
                        };
                        if ui
                            .selectable_label(
                                self.settings.two_model == m.id,
                                egui::RichText::new(label).small(),
                            )
                            .clicked()
                        {
                            pick = Some(m.id.clone());
                        }
                    }
                });
        }
        if let Some(id) = pick {
            self.settings.two_model = id;
        }
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("system".into(), Arc::new(egui::FontData::from_owned(bytes)));
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.insert(0, "system".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// How a window is named in the picker: the desktop gets its own name instead of
/// Explorer's internal "Program Manager".
fn window_label(w: &WindowInfo, max: usize) -> String {
    if w.exe.eq_ignore_ascii_case("explorer.exe") && w.title == "Program Manager" {
        return "Pulpit (explorer.exe)".into();
    }
    format!("{} ({})", short(&w.title, max), w.exe)
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::Done => "✔ Cel osiągnięty".into(),
        Outcome::Preview => "Podgląd: pokazano pierwszą decyzję, nic nie wstrzyknięto".into(),
        Outcome::Budget => "Wyczerpany limit kroków".into(),
        Outcome::Uncertain(r) => format!("Niepewne — {r}"),
        Outcome::NeedsText => {
            "Potrzebny tekst do wpisania (cudzysłów w celu albo pole „Tekst”)".into()
        }
        Outcome::Blocked(r) => format!("Zablokowane — {r}"),
        Outcome::Stalled => "Brak zmian na ekranie po akcjach".into(),
        Outcome::Killed => "Kill-switch Ctrl+Alt+K".into(),
        Outcome::Stopped => "Zatrzymane".into(),
        Outcome::FocusLost(r) => format!("Ktoś inny wciąż przejmuje fokus — przerwane ({r})"),
        Outcome::TargetGone => "Okno zniknęło bez udziału pętli".into(),
    }
}

fn action_text(a: &Action) -> String {
    match a {
        Action::Click {
            name, x, y, right, ..
        } => format!(
            "{} {name} @({x},{y})",
            if *right { "prawy klik" } else { "klik" }
        ),
        Action::Type {
            text, target_name, ..
        } => format!(
            "wpisz „{text}” → {}",
            target_name.as_deref().unwrap_or("aktywna kontrolka")
        ),
        Action::Key { key } => format!("klawisz {key}"),
        Action::Scroll { notches, .. } => {
            if *notches > 0 {
                "przewiń w dół".into()
            } else {
                "przewiń w górę".into()
            }
        }
        Action::Wait => "czekaj".into(),
        Action::Switch { title, exe, .. } => format!("przełącz na „{}” ({exe})", short(title, 40)),
        Action::ShowDesktop => "pokaż pulpit (minimalizuje okna z wierzchu)".into(),
    }
}

fn widen_text(rung: u8) -> &'static str {
    match rung {
        1 => "kontekst — etykiety i więcej kandydatów",
        2 => "przegląd otwartych okien",
        3 => "System Two",
        _ => "—",
    }
}

fn survey_text(v: &serde_json::Value) -> String {
    v["top"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(format!(
                        "{} {:.2}",
                        e.get(0)?.as_str()?,
                        e.get(1)?.as_f64()?
                    ))
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default()
}

fn step_card(ui: &mut egui::Ui, r: &StepRecord) {
    let (verdict, color) = match &r.verdict {
        Verdict::Done => (
            "✔ cel osiągnięty".to_string(),
            egui::Color32::from_rgb(120, 220, 140),
        ),
        Verdict::Act { action } => (
            format!(
                "{} {}",
                action_text(action),
                if r.executed { "✓" } else { "[podgląd]" }
            ),
            if r.executed {
                egui::Color32::from_rgb(120, 180, 255)
            } else {
                egui::Color32::GRAY
            },
        ),
        Verdict::Uncertain { reason, .. } => (
            format!("niepewne — {reason}"),
            egui::Color32::from_rgb(255, 200, 90),
        ),
        Verdict::NeedsText => (
            "potrzebny tekst".to_string(),
            egui::Color32::from_rgb(255, 200, 90),
        ),
        Verdict::Blocked { reason } => (
            format!("zablokowane — {reason}"),
            egui::Color32::from_rgb(255, 110, 110),
        ),
    };
    let s = &r.signals;
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(format!("#{}", r.step)).strong());
            ui.label(format!("{} „{}”", r.app, short(&r.title, 40)));
            if r.changed == Some(false) {
                ui.label(egui::RichText::new("bez zmian").weak());
            }
            ui.label(
                egui::RichText::new(format!(
                    "· skan {:.0} ms ({} el.) · Jev {:.0} ms{} · ${:.5}",
                    r.scan_ms,
                    r.sent_elements,
                    r.jev_ms,
                    if r.narrowed { " (×2, zawężenie)" } else { "" },
                    r.cost_usd
                ))
                .weak(),
            );
        });
        if r.widen > 0 {
            ui.label(
                egui::RichText::new(format!("poszerzanie: {}", widen_text(r.widen)))
                    .color(WIDEN_COLOR)
                    .small(),
            );
        }
        if let Some(sv) = &r.survey {
            ui.label(
                egui::RichText::new(format!("przegląd okien: {}", survey_text(sv)))
                    .color(WIDEN_COLOR)
                    .small(),
            );
        }
        if let Some(sg) = &r.subgoal {
            ui.label(
                egui::RichText::new(format!("podcel {sg}"))
                    .color(TWO_COLOR)
                    .small(),
            );
        }
        ui.colored_label(color, format!("→ {verdict}"));
        for t in &r.two {
            ui.label(
                egui::RichText::new(format!(
                    "System Two ({}, {}, {:.1} s, ${:.4}){}: {}",
                    match t.kind {
                        uc_two::Kind::Plan => "plan",
                        uc_two::Kind::Rescue => "ratunek",
                    },
                    t.model,
                    t.ms / 1000.0,
                    t.cost_usd,
                    if t.applied { " ✓" } else { "" },
                    t.note
                ))
                .color(TWO_COLOR)
                .small(),
            );
        }
        ui.label(
            egui::RichText::new(format!(
                "cel {:.2} · tekst {:.2} · destrukcyjne {:.2} · target {} {} ({:.2}, gap {:.2}) · op {} {:.2}",
                s.goal_reached,
                s.needs_text,
                s.is_destructive,
                s.target,
                s.target_name.as_deref().unwrap_or("-"),
                s.target_conf,
                s.target_gap,
                s.op,
                s.op_conf
            ))
            .weak()
            .small(),
        );
    });
}
