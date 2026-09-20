# Architektura Jev-UltraCuse — najszybszy Computer Use na Windows

Data: 2026-09-20. Zależy od: [01 — analiza Jev](01-jev-computer-use-analiza.md), [ADR-001 język](02-ADR-jezyk.md), [ADR-002 STT](04-STT-dostepnosc.md).

## 1. Cel i miara

Jeden przenośny `ultracuse.exe`, który obsługuje komputer głosem i klawiaturą/myszą „jak urządzenie HID", z decyzjami Jev w pętli. Miara „najszybszy":

| Metryka | Cel | Odniesienie |
|---|---|---|
| Krok pętli (percepcja → decyzja → akcja → settle), okno z UIA | **≤ 500 ms p50**, ≤ 700 ms p95 | 0.95 s typesafe-computer-use; 567 ms „Correct but Late" |
| Ostatnie słowo komendy głosowej → akcja | **≤ 600 ms** (STT final ≤ 250 ms + Jev ~300 ms + akcja) | jev-voice-browser ~300 ms po Web Speech |
| Powtórzone zadanie (cache makr) | 0 wywołań Jev | — |
| Start binarki do gotowości (bez modelu STT) | < 150 ms; z modelem STT < 1.5 s | Python onefile 1.5–4 s |
| Koszt kroku | ≤ $0.0003 | cu_bench |

Każda liczba w README pochodzi ze skryptu w `bench/` (zasada z jevskill).

## 2. Proces i wątki

```
ultracuse.exe (jeden proces, Rust)
├─ [T0] pompa komunikatów Win32: tray, globalny hotkey (push-to-talk / kill-switch Ctrl+Alt+K), okno statusu
├─ [T1] audio: WASAPI capture 16 kHz mono → ring buffer → VAD → STT streaming (partiale / finał)   ← ADR-002
├─ [T2] UIA (COM STA, jeden na proces): FindAllBuildCache + CacheRequest, subskrypcje zdarzeń, hash drzewa
├─ [T3] tokio runtime: klient Jev (h2 keep-alive, hedging), ledger, Named Pipe `--serve`
├─ [T4] executor: SendInput (batch), ciepła sesja pwsh (potok), schowek
└─ [T5] OCR/grounding (opcjonalny, asynchroniczny): tylko regiony bez UIA, wynik do następnego kroku
```

Zasady: COM tylko na T2 (typy `!Send`); żadnej alokacji ani I/O w ścieżce `SendInput`; ledger i logi przez kanał do T3; kill-switch sprawdzany przez `GetAsyncKeyState` w T0 **bez hooków** (LL-hook dodaje opóźnienie systemowi i ma timeout).

## 3. Pętla kroku (gorąca ścieżka)

```
      ┌────────────────────────────────────────────────────────────────────┐
      │ 1. PERCEPCJA  5–60 ms                                              │
      │    Win32: foreground hwnd, tytuł, exe, kursor (µs)                 │
      │    UIA: jedno FindAllBuildCache(Descendants, OR(ControlType…),     │
      │         CacheRequest{Name, AutomationId, ControlType, Rect,        │
      │         IsEnabled, IsOffscreen, Value, Patterns})                  │
      │    → elementy [{i, role, name, box, val, enabled}]                 │
      ├────────────────────────────────────────────────────────────────────┤
      │ 2. REDUKCJA + HASH  ≤ 2 ms (kod)                                   │
      │    widoczne ∧ enabled ∧ rola interaktywna; dedupe (role,name);     │
      │    priorytet: rola, odległość od kursora/fokusu; cap 60;           │
      │    nazwy ≤ 48 zn.; reindeks e0..eN; hash(normalized) → stuck/diff  │
      ├────────────────────────────────────────────────────────────────────┤
      │ 3. CACHE MAKR  0 ms                                                │
      │    (goal_norm, hash_norm) → decyzja z poprzedniego runu            │
      ├────────────────────────────────────────────────────────────────────┤
      │ 4. JEV  ~290–330 ms (1 POST, hedged po 400 ms, timeout 1.5 s)      │
      │    state = {goal, scene, elements, last, dictated?}                │
      │    questions = {target, op, goal_reached, needs_text,              │
      │                 is_destructive, progress, recovery}                │
      ├────────────────────────────────────────────────────────────────────┤
      │ 5. WALIDACJA  < 1 ms (kod)                                         │
      │    element istnieje ∧ box na ekranie ∧ op ∈ dozwolone(rola)        │
      │    ∧ conf ≥ próg(typ akcji) ∧ (nieodwracalna ⇒ potwierdzenie)      │
      ├────────────────────────────────────────────────────────────────────┤
      │ 6. AKCJA  < 5 ms                                                   │
      │    UIA Invoke/SetValue/Select gdy pattern jest, inaczej SendInput  │
      │    (cała sekwencja w jednym wywołaniu; tekst ≥16 zn. przez schowek)│
      ├────────────────────────────────────────────────────────────────────┤
      │ 7. SETTLE  0–200 ms                                                │
      │    czekaj na zdarzenie UIA (StructureChanged/PropertyChanged)      │
      │    lub zmianę hasha; cap 200 ms; combobox 200 ms                   │
      └────────────────────────────────────────────────────────────────────┘
      stop: goal_reached ≥ 0.85 (+ niezależna weryfikacja kodem) ∨ target=none ∨
            conf < 0.6 ∨ 2× ten sam hash po akcji ∨ budżet kroków/czasu ∨ kill-switch
```

Bundle pytań (jeden plik stałych `decide/consts.rs`, wg cookbooka `function_calling` i ADR jev-ultrafast):

| Pytanie | Typ | Klucze / skala | Próg (kod) |
|---|---|---|---|
| `target` | choice | `e0…eN`, `none` | conf ≥ 0.6; gap top-2 ≥ 0.15, inaczej kaskada region→element |
| `op` | choice | `click, double, right, type, key, scroll, wait, done, escalate` | op ∈ dozwolone(rola) |
| `goal_reached` | noul | — | ≥ 0.85 **i** weryfikacja kodem (tytuł/pole/plik) |
| `needs_text` | noul | — | ≥ 0.5 → weź ostatni dyktowany tekst / poproś głosem |
| `is_destructive` | noul | — | doradcze; lista `Delete/Usuń/Send/Wyślij/Pay/Zapłać/Format/Uninstall/…` decyduje; ≥ 0.85 lub lista ⇒ potwierdzenie głosem |
| `progress` | score | 5 etapów | telemetria; spadek 2× ⇒ recovery |
| `recovery` | choice | `retry, dismiss, scroll, escalate` | czytane tylko gdy hash bez zmian po akcji |

Progi per typ akcji (wzorzec confidence-routing): klik/wpisanie 0.6 · klawisz systemowy (Win, Alt+F4) 0.75 · akcja nieodwracalna 0.85 + potwierdzenie zawsze.

## 4. Tor głosowy (dostępność)

```
mikrofon → VAD → STT (partiale co ~100–200 ms, finał po ciszy 300 ms)
   → debounce 200 ms → Jev fan-out na partialu:
        is_command (czy mówi do komputera) · intent (13: klik/wpisz/otwórz/przewiń/zamknij/
        zapisz/potwierdź/anuluj/powtórz/dyktuj/stop/pomoc/inne) · target (e0..eN, none) ·
        complete (czy komenda skończona) · destructive · scroll_amount (mało/strona/koniec)
   → gdy complete ≥ 0.7 ∧ conf ≥ próg: wykonaj bez czekania na finał STT
   → tryb DYKTOWANIA: „dyktuj" włącza, tekst finalny STT idzie do pola (SetValue lub schowek),
     „koniec dyktowania" wyłącza; Jev nie generuje tekstu — użytkownik go mówi
   → niejednoznaczny target: numerowane etykiety na ekranie (overlay), użytkownik mówi numer
   → destrukcyjne: „potwierdź / anuluj" głosem (toast + TTS krótkie potwierdzenie)
```

Ustawienia dostępności: push-to-talk **lub** ciągły nasłuch ze słowem-kluczem; powolna mowa (VAD 600 ms); wysoki kontrast overlay; wszystkie komendy dostępne też z klawiatury; brak wymogu precyzji myszy.

## 5. Percepcja — źródła i kolejność

| Źródło | Koszt | Kiedy |
|---|---|---|
| Win32 (`GetForegroundWindow`, `GetWindowTextW`, `GetCursorPos`, `EnumWindows`) | µs | zawsze, co krok |
| UIA CacheRequest (aktywne okno) | 5–60 ms natywne; 110–200 ms Electron/Chrome | zawsze; drzewo cache'owane, invalidacja zdarzeniami |
| UIA zdarzenia (`IUIAutomation6::CreateEventHandlerGroup`, `CoalesceEvents`) | 0 gdy nic się nie dzieje | settle, wake-up, invalidacja cache |
| Przeglądarka: UIA na `Chrome_RenderWidgetHostHWND` (sesja użytkownika, zero konfiguracji) | 50–200 ms | domyślnie dla Chrome/Edge użytkownika |
| Przeglądarka: CDP `DOMSnapshot.captureSnapshot` (własny profil `--user-data-dir`, Chrome ≥136 ignoruje flagę na profilu domyślnym) | kilka ms | zadania automatyczne w zarządzanej przeglądarce (v2) |
| OCR (`Windows.Media.Ocr` przez WinRT z Rust, lokalny) | 10–100 ms | asynchronicznie, tylko regiony bez UIA (canvas, gry, RDP) |
| Capture (`windows-capture` / DXGI DDA) | 1–8 ms | wejście OCR + diff pikseli jako sygnał settle; **nie** w gorącej ścieżce |

Redukcja wg JevUse `reduce.py` (sprawdzona): role `CORE_INTERACTIVE`, warunkowe `custom/pane/generic` tylko z nazwą, `UNNAMED_OK` dla pól formularza, wagi ról, cap, truncacja.

## 6. Executor — „jak urządzenie HID"

- `SendInput` z **jedną tablicą na całą akcję**: ruch absolutny (`MOUSEEVENTF_ABSOLUTE|VIRTUALDESK`, współrzędne 0–65535 wirtualnego pulpitu, DPI-aware per-monitor v2) + down/up; chordy w jednym wywołaniu; znaki ASCII przez **scan-code** (`MapVirtualKeyW`), diakrytyka przez `KEYEVENTF_UNICODE`; tekst ≥ 16 znaków przez schowek + Ctrl+V (`CF_UNICODETEXT`, `GlobalAlloc` z poprawnymi prototypami 64-bit).
- Preferencja **wzorców UIA** (`Invoke`, `Value.SetValue`, `SelectionItem.Select`, `Toggle`, `Scroll`) — działają bez ruchu kursora i bez ryzyka trafienia obok; SendInput jako fallback i dla aplikacji bez wzorców.
- Drag: sekwencja punktów w jednym `SendInput` (≥ 8 kroków).
- Scroll: `MOUSEEVENTF_WHEEL` ±120 × N, po `scroll_amount` z Jev (mało=3, strona=10, koniec=Ctrl+End).
- Kill-switch globalny **Ctrl+Alt+K** (bez hooka) rozbraja executor natychmiast; uzbrojenie (`--act`) osobne od podglądu.
- Driver Interception — poza v1 (test-signing, anti-cheat).

## 7. PowerShell — ciepła sesja

Zamiast spawn per komenda (300–600 ms): jeden proces `pwsh -NoProfile -NonInteractive -NoLogo -Command -` trzymany przez cały run, komendy przez stdin, wynik ograniczony sentinelami (`<<<UC:{id}>>>`), timeout per komenda, kill przy przekroczeniu. Latencja komendy ≈ jej własny czas (`Get-ChildItem` ~5–30 ms).

Bramka Tier 0 przed wykonaniem: komenda musi być ASCII bez polskich czasowników GUI; lista destrukcyjnych (`Remove-Item`, `Format-Volume`, `Stop-Computer`, `reg delete`, `diskpart`, …) → zawsze potwierdzenie głosem; brak `-Force` bez potwierdzenia. Jev pyta `use_tool` (`ps / search / grep / read / none`) z kalibracją p50 ms w kryteriach (wzorzec z JevUse `_tool_choice_question`).

## 8. Klient Jev (gorący)

- Provider vendor-first (`api.typesafe.ai/v1/systemone`, model **pinowany** `jev-1.13.0`), fallback OpenRouter (`/api/alpha/decisions`, `typesafe/jev-1.13`); klucze z env **i** `HKCU\Environment` (`JEV_API_KEY`, `TYPESAFE_API_KEY`, `OPENROUTER_API_KEY`, `JEVUSE_API_KEY`).
- `reqwest` (rustls, http2 prior knowledge gdzie możliwe, keep-alive), nagłówki raz; body = prefiks + state + pytania (pytania skompilowane do bajtów per snapshot).
- Rozgrzewka **mini-decyzją** (HEAD na vendorze ~600 ms i nie gwarantuje keep-alive), odświeżanie co 45 s.
- **Hedging**: drugi identyczny POST po 400 ms, wygrywa pierwszy, drugi anulowany; koszt 2× tylko na ogonie; raportowany osobno.
- Timeout 1.5 s; retry tylko 429/529 z backoffem i `retry-after`; 422 = błąd naszego payloadu (loguj body, nie ponawiaj).
- `session_id`/`trace` tylko dla OpenRouter.
- Ledger (JSONL) przez kanał, zapis w tle: stage timings, tokeny, koszt (vendor: liczony z ceny), rozkłady, hash stanu, decyzja, wynik weryfikacji.

## 9. System Two (rzadko)

Wywoływany tylko: `target=none` 2× ∨ Hn > 0.6 przez 2 kroki ∨ użytkownik prosi „napisz…". Dostaje `GuiState` + rozkład Jev jako prior, zwraca **jeden wiersz** planu (3–5 kroków) albo tekst. Model przez OpenRouter (mały: `google/gemini-2.5-flash-lite` lub `inception/mercury-2.5`), max 3 wywołania/zadanie. Bez Systemu Two aplikacja działa (dyktowanie + Jev).

## 10. Bezpieczeństwo i prywatność

- Do Jev idzie **tylko tekst z aktywnego okna** po redukcji (nazwy kontrolek, tytuł, wartości pól ≤ 24 zn.); nigdy hasła: pola `IsPassword` maskowane w kodzie; zrzuty ekranu nie opuszczają maszyny.
- Klucze API: env/rejestr, w pamięci procesu; log nigdy nie zawiera fragmentów (fingerprint SHA-256[:8]).
- Tekst z UI może zawierać instrukcje dla modelu (jaggedness #8): guardy destrukcyjne i lista PS są w kodzie, nie w pytaniach.
- Tryb podglądu domyślny (`would click …`); `--act` uzbraja; kill-switch zawsze.

## 11. Układ repozytorium (Rust workspace)

```
Cargo.toml                 workspace, profile.release: lto=fat, codegen-units=1, panic=abort, strip
crates/
  uc-win32/    Win32: okna, kursor, DPI, schowek, wirtualny pulpit (windows-rs)
  uc-uia/      UIA COM STA: CacheRequest, FindAllBuildCache, zdarzenia, wzorce; GuiState + reduce + hash
  uc-input/    SendInput batch, scan-code/unicode, drag, scroll, kill-switch
  uc-shell/    ciepła sesja pwsh, guard Tier 0, narzędzia search/grep/read
  uc-jev/      klient (vendor/OpenRouter), pytania, hedging, ledger, entropia
  uc-voice/    WASAPI capture, VAD, STT streaming (ADR-002), intent fan-out
  uc-loop/     pętla kroku, cache makr, progi, eskalacja, System Two
  ultracuse/   binarka: tray, hotkeys, okno statusu, CLI (doctor/probe/run/serve/bench)
bench/         skrypty pomiarowe (Rust bin + Python lab przez --serve) i wyniki JSON
docs/          ta dokumentacja, ADR-y
lab/           (opcjonalnie) linki do D:\code\JevUse — eksperymenty Python
```

## 12. Plan pomiarowy (go/no-go, przed feature'ami)

| # | Eksperyment | Rozstrzyga |
|---|---|---|
| R1 | UIA Rust vs Python: ten sam skan (Notatnik, Eksplorator, Ustawienia, Chrome, VS Code) ×20 | czy natywny klient cokolwiek zmienia w percepcji (oczekiwanie: nie; RPC dominuje) |
| R2 | Jev z Rust: p50/p95 vendor vs OpenRouter, plain vs hedged, N=12/30/60/120 | próg hedgingu, wybór końcówki |
| R3 | STT: ostatnie słowo → finał, p50/p95, PL, 3 silniki (ADR-002) | silnik domyślny |
| R4 | E2E 10 zadań × 3 runy (Notatnik zapisz jako; Eksplorator nowy folder; Ustawienia tryb ciemny; Chrome szukaj; Kalkulator 12×34; …): czas, kroki, koszt, sukces, eskalacje; te same zadania na kofanlabs | „najszybszy" — albo nie |
| R5 | Kryteria PL vs EN na 30 ekranach | język pytań |
| R6 | Kalibracja: pary decyzja→wynik, ECE per pytanie | progi 0.6/0.85 → własne |
