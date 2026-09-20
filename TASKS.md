# TASKS

Zajęcie zadania = wpis `claimed_by` + gałąź `agent/<sesja>/<slug>` w pierwszym commicie. Kryteria i pomiary: [docs/03-architektura.md](docs/03-architektura.md) §12, [docs/04-STT-dostepnosc.md](docs/04-STT-dostepnosc.md) §4. Każda liczba w README musi pochodzić ze skryptu w `bench/`.

| id | faza | zadanie | kryterium | status | claimed_by |
|---|---|---|---|---|---|
| 0.1 | 0 | Research docs Jev, ADR-001 język, ADR-002 STT, architektura | pliki w `docs/` | done 2026-09-20 | fable |
| 0.2 | 0 | Scaffold workspace Rust: `uc-win32`, `uc-uia`, `uc-input`, `uc-shell`, `uc-jev`, stuby `uc-voice`, `uc-loop`, CLI `ultracuse` | `cargo check` zielone | done 2026-09-20 | fable |
| 0.3 | 0 | R1: `ultracuse bench-uia` vs JevUse Python — Notatnik / Chrome / Claude / MLRC done; **do zrobienia**: Eksplorator / Ustawienia / Kalkulator / VS Code / Word | tabela w `bench/R1-uia.md` | in progress | fable |
| 0.4 | 0 | R2: `ultracuse bench-jev` vendor i OpenRouter w jednym oknie, plain + hedged, N=12/30/60/120; vendor domyślny, OpenRouter opcjonalny (`UC_PROVIDER`, `--provider`) | `bench/R2-jev.md` serie A i B | done 2026-09-20 | fable |
| 0.6 | 0 | Hedge adaptacyjny: straż 600 ms (2×p50) już w `consts`; próg z p99 okna, test z wymuszonym zawieszeniem (proxy z opóźnieniem), 50 wywołań hedged vs plain; N=240 | `hedge_wins` > 0 przy stallu, p95 hedged ≤ p95 plain, koszt ≤ +10 % | todo | |
| 0.5 | 0 | Zdalne repo (GitHub `hexart/Jev-UltraCuse`), `main` chroniony, CI: `cargo fmt --check`, `clippy -D warnings`, `test`, `build --release` na `windows-latest` | zielony pierwszy PR | todo | |
| 1.1 | 1 | `uc-uia`: subskrypcja zdarzeń (`IUIAutomation6::CreateEventHandlerGroup`, StructureChanged/PropertyChanged) → settle po zdarzeniu lub zmianie hasha, cap 200 ms | test na Notatniku: settle ≤ 50 ms po kliknięciu | todo | |
| 1.2 | 1 | `uc-uia`: wzorce `Invoke`/`Value.SetValue`/`SelectionItem`/`Toggle` przez cache (`AddPattern`) i `act_uia(el, op)` z fallbackiem do `uc-input` | E2E na WinForms z JevUse `scripts/e2e_target.ps1` | todo | |
| 1.3 | 1 | `uc-jev`: ledger JSONL poza gorącą ścieżką (kanał + wątek), pary decyzja→wynik, `stats` | 1 000 decyzji bez utraty rekordu; `act` < 0.5 ms | todo | |
| 1.4 | 1 | `uc-loop`: pętla kroku wg §3 architektury (perceive → reduce → cache → decide → validate → act → settle), stop-warunki, log kroku `runs/<ts>/` | dry-run na 5 zadaniach z logiem czasów etapów | todo | |
| 1.5 | 1 | `uc-loop`: cache makr `(goal_norm, hash_norm)` → decyzja; invalidacja gdy hash bez zmian po akcji | powtórka zadania: 0 wywołań Jev | todo | |
| 1.6 | 1 | Guardy Tier 0: lista nieodwracalnych (nazwy kontrolek + PS), maskowanie `IsPassword`, injection z UI nie steruje guardami | testy jednostkowe: żadna akcja z listy bez potwierdzenia | todo | |
| 1.7 | 1 | E2E 10 zadań × 3 runy (Notatnik zapisz jako; Eksplorator nowy folder + nazwa; Ustawienia tryb ciemny; Chrome szukaj; Kalkulator 12×34; …) — czas, kroki, koszt, sukces, eskalacje; te same na kofanlabs | `bench/R4-e2e.json` + tabela w README | todo | |
| 2.1 | 2 | `uc-voice`: WASAPI capture (`cpal`), VAD, `SttEngine` dla ElevenLabs Scribe v2 Realtime (WS) | R3.1 | todo | |
| 2.2 | 2 | `uc-voice`: `SttEngine` whisper.cpp (`whisper-rs`, GPU/CPU), re-dekodowanie bufora, downloader modeli z SHA-256 | R3.2 | todo | |
| 2.3 | 2 | Intent fan-out na partialu (`is_command`, `intent`, `target`, `complete`, `destructive`, `scroll_amount`), debounce 200 ms; tryb dyktowania; overlay z numerami przy niejednoznaczności | „ostatnie słowo → akcja" ≤ 600 ms p50 online | todo | |
| 2.4 | 2 | Potwierdzenia głosem/klawiszem dla nieodwracalnych; TTS krótkich komunikatów (SAPI offline / ElevenLabs) | test: „usuń plik" bez „potwierdź" nic nie robi | todo | |
| 2.5 | 2 | Nemotron-3.5-ASR-Streaming: eksport ONNX, `ort`, pomiar R3.3, licencja | decyzja: zastępuje whisper offline lub nie | todo | |
| 3.1 | 3 | Tray + globalne hotkeye (`tray-icon`, `global-hotkey`), okno statusu (partiale, decyzja, kill-switch), tryb podgląd/uzbrojony | działa bez konsoli | todo | |
| 3.2 | 3 | Portable exe: `lto`, `strip`, statyczny CRT, podpis Authenticode; rozmiar i zimny start w `bench/` | exe < 15 MB (bez modeli), start < 150 ms | todo | |
| 3.3 | 3 | OCR asynchroniczny (`Windows.Media.Ocr` przez WinRT) dla regionów bez UIA; licznik kroków na fallbacku per aplikacja | Spotify/canvas: działa w trybie zdegradowanym | todo | |
| 3.4 | 3 | System Two: plan 3–5 kroków na starcie + tekst na żądanie (mały LLM przez OpenRouter), max 3/zadanie | średnia liczba wywołań Jev/zadanie −30 % bez spadku sukcesu | todo | |
| 3.5 | 3 | Kryteria PL vs EN (R5) i kalibracja progów (R6) | progi własne zamiast 0.6/0.85 | todo | |
| 3.6 | 3 | `--serve` (Named Pipe, JSON) dla laboratorium Python (JevUse) | te same benchmarki z Pythona i Rusta 1:1 | todo | |
