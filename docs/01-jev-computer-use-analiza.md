# Jev 1.13 pod kątem Computer Use — analiza dokumentacji

Data: 2026-09-20. Źródła: oficjalne docs TypeSafe (`docs.typesafe.ai/llms.txt` — 109 stron), strona modelu na OpenRouter, karta „model jaggedness", 18 cookbooków, pomiary z żywego API wykonane w `D:\code\JevUse` (2026-09-19) i `D:\code\jevskill` (2026-09-20), oraz README wszystkich istniejących implementacji Computer Use na Jev. Każda liczba ma źródło; rzeczy niepotwierdzone oznaczone **INFERENCJA**.

---

## 0. TL;DR

Jev to model **decyzji, nie tekstu**: dostaje tekstowy `state` i N typowanych pytań (`noul` tak/nie, `choice` jedno z K, `score` punkt na skali) i zwraca **rozkłady prawdopodobieństwa**, wszystkie pytania równolegle, w jednym round-tripie ~300 ms z Polski. Nie widzi obrazu, nie pisze tekstu, nie planuje, nie liczy, nie porównuje dwóch stanów.

Dla Computer Use oznacza to jeden konkretny podział pracy:

| Kto | Co robi w pętli |
|---|---|
| **Kod (Tier 0, 0 ms)** | percepcja (UIA/Win32), redukcja kandydatów, diff stanu przed/po, liczenie, daty, lista akcji nieodwracalnych, walidacja odpowiedzi |
| **Jev (System One, ~300 ms)** | „który element", „jaka operacja", „czy cel osiągnięty", „czy to niebezpieczne", „czy trzeba tekstu" — wszystko w JEDNYM wywołaniu |
| **STT (dyktowanie)** | **treść do wpisania w pola** — użytkownik ją mówi; Jev nie musi generować tekstu |
| **LLM (System Two, rzadko)** | plan wieloetapowy, tekst którego użytkownik nie podyktował, odblokowanie gdy Jev ma wysoką entropię |

Wąskim gardłem „najszybszego Computer Use" **nie jest język programowania ani UIA** — jest (a) round-trip Jev (~250–300 ms samej inferencji u providera, stałe, niezależne od rozmiaru state) i (b) **liczba kroków** na zadanie. Dźwignie: batch pytań (darmowy), diff w kodzie, cache makr, spekulacyjny plan, hedging ogona latencji, końcówka vendora zamiast agregatora (−30…−60 ms).

---

## 1. Kontrakt API — fakty (co wolno, czego nie)

| Fakt | Wartość | Źródło |
|---|---|---|
| Endpoint vendora | `POST https://api.typesafe.ai/v1/systemone`, model `jev-latest` (→ `jev-1.13.0`), aliasy `jev-preview` | `/api.md`, `/models.md` |
| Endpoint OpenRouter | `POST https://openrouter.ai/api/alpha/decisions`, model `typesafe/jev-1.13` (permaslug `typesafe/jev-1.13-20260917` — przyjmowany też jako `model` w żądaniu, 200 potwierdzone 2026-09-20), alias `~typesafe/jev-latest` | strona modelu OR, `jev-1.13-openrouter.md` |
| Cena | **$0.042 / 1M tokenów wejścia, wyjście $0** — identyczna na obu końcówkach | `/models.md`, OR |
| Kontekst | vendor: **64k/request, 32k na `state` + najdłuższe pytanie**; OpenRouter: 32k | `/models.md` |
| Rate limit (vendor) | 250 000 tok/s, 1 200 req/min, **„adjusting dynamically"** | `/models.md` |
| Choice | max **255 opcji** (dokładność na sąsiednich opcjach spada dużo wcześniej; skill jevskill ostrzega >40, pomiar CU stabilny do 60) | `/api.md`, `cu_bench` |
| Score | 2–10 poziomów | `/api.md` |
| Wejście | **tylko tekst**: string / obiekt JSON / tablica; brak obrazu i audio | `/concepts/state.md` |
| Wyjście | `noul` → P(true); `choice` → `choice`+`confidence`+`probabilities`; `score` → float (ułamkowy!)+`confidence`+`probabilities`+`legend` | `/api.md` |
| Reasoning / tools / streaming | brak, brak, brak — pełny RTT zanim padnie odpowiedź | OR model card |
| Błędy | vendor: 401, **422** (walidacja), 429, 529; OR: 400, 401, 402, 413, 429, 502, 529 | `/api.md`, OR |
| Retry | 429/529 → wykładniczy backoff; nigdy natychmiast | `/api.md` |
| `session_id`, `user`, `provider`, `trace` | **tylko OpenRouter** — vendor zwraca 400 `api_usage_error` (potwierdzone na żywo 2026-09-20) | `research-2026-09-20-jev-cu.md` §1.5 |
| `usage.cost` | OpenRouter raportuje (sprawdzone: 344 tok → $0.0000144 = $0.042/Mtok); **vendor nie** — liczyć z ceny | `api.md` (jevskill) |
| SDK | oficjalne: `pip install typesafe_sdk` (`TypeSafeClient.system_one(state, questions)`), `npm i @typesafe-ai/sdk`; community: Go, Java. **Brak Rust** — docs: „You can also call the HTTP API directly from any language." | `/sdk.md` |
| Dostęp | OpenRouter: natychmiast z kredytami; vendor: waitlista, klucze partiami | `api.md` (jevskill) |
| Alias `jev-latest` | **przesuwa się** — odpowiedź zawiera versioned id; progi kalibrowane pod wersję → pinować | `api.md` (jevskill) |

Konsekwencje architektoniczne (twarde):

1. **Cała percepcja musi stać się tekstem** (UIA, Win32, OCR, DOM) — screenshot nigdy nie trafia do Jev.
2. **Jeden `state` na request** — pytania wymagające innego snapshotu (przed vs po akcji) to osobne requesty. Batch kroku = jedna spójna klatka percepcji.
3. **Brak streamingu** → jedyne dźwignie latencji po stronie klienta: ciepłe połączenie, batch, mniej kroków, hedging, bliższa końcówka.
4. **Jeden provider (TypeSafe)** — brak failoveru na inny model decyzyjny; awaria = degradacja do LLM/człowieka.
5. **Alpha API** na OpenRouter — schema może się zmienić; klient za adapterem.

---

## 2. Trzy prymitywy w pętli Computer Use

| Punkt decyzyjny kroku | Typ | Pytanie (kształt) | Kto konsumuje odpowiedź |
|---|---|---|---|
| Który element? | `choice` | klucze = **stabilne ID elementów** ze snapshotu (`e0…eN`) + **`none`** | executor: ID → bbox → `SendInput` / `Invoke` |
| Jaka operacja? | `choice` | `click / type / key / scroll / wait / done / escalate` (słownik zamknięty) | kod waliduje `op ∈ dozwolone(rola)` |
| Cel osiągnięty? | `noul` | „Czy `goal` jest już spełniony przez widoczny stan?" | próg lokalny; `done` **wymaga niezależnej weryfikacji** (jev-ultrafast) |
| Trzeba tekstu? | `noul` | „Czy następny krok wymaga wpisania treści?" | odpala STT/LLM równolegle |
| Akcja nieodwracalna? | `noul` | „Czy ta operacja usuwa dane / wysyła / płaci?" | **doradcza**; decyduje deterministyczna lista + próg 0.85 |
| Postęp | `score` | 5 etapów zadania | telemetria, wykrywanie stagnacji |
| Recovery | `choice` | `retry / dismiss / scroll / escalate` | deterministyczna akcja |

Zasady wyprowadzone z semantyki API (JevUse §6.4, potwierdzone pomiarem):

- **Jeden request na krok.** 8 pytań = 1 RTT (E2: 305 ms batch vs 2 394 ms sekwencyjnie, 7.84×; jevskill E3: 12.4×; vendor: 13 pytań 0.27 s vs 2.71 s).
- **Nazywaj wartość w pytaniu** ścieżką w backtickach (`` `elements.e3` ``, `` `goal` ``) — bez tego rozkłady są płaskie (~0.74 dla wszystkiego). Indeksy tablic działają (`tickets[0].message`).
- **Klucze `choice` = ID, opisy kryteriów = testy akceptacyjne** (rola + nazwa + stan + zawartość pola), nie subiektywne przymiotniki.
- **Zawsze `none` / `escalate`** jako furtka — model nie zaproponuje opcji, której nie dostał.
- **Próg porównuj lokalnie** — jedna odpowiedź `noul` obsługuje 0.5 (zwykła bramka) i 0.9 (guard) bez drugiego requestu.
- **`score` nie służy do rankingu elementów** — do tego jest `choice` z rozkładem.

---

## 3. Zmierzone liczby (nie marketing)

### 3.1 Latencja i koszt jednego wywołania

| Pomiar | Wartość | Źródło |
|---|---|---|
| Vendor deklaruje | 70–500 ms end-to-end „depending on where the caller is"; „adding questions barely changes the response time" | docs |
| JevUse E1 (PL → OpenRouter, httpx/h2) | **P50 300–330 ms**, P95 420–535, best 265; sieć TCP/TLS ~60–70 ms → **inferencja ≈ 250–300 ms stała** | `JevUse/bench/e1.jsonl` |
| JevUse E1 vs rozmiar state | 300 → 11 400 tok: Δ P50 ≈ 40 ms (płasko) | j.w. |
| jevskill E1 | cold 485 ms, warm p50 325, p95 427, rozrzut 299–683 na identycznym wejściu | `jevskill/bench/results.json` |
| jevskill E2 | 324 tok → 353 ms; 920 → 370; 7 020 → 468 (słaba zależność, dominuje szum sieci; koszt liniowy) | j.w. |
| **cu_bench: drzewo UI + 4 pytania** (OpenRouter) | N=12: 367 ms (1 723 tok, $0.000072); N=30: 325 ms; N=60: 371 ms — wybór celu 0.99 do N=60 | `jevskill/bench/cu_bench.py` |
| **cu_bench na końcówce vendora** | N=12: **304 ms**; N=30: **292**; N=60: **320** — o hop mniej: **−33…−63 ms, węższy ogon** (max 333–401 vs 431–485) | j.w., 2026-09-20 |
| warm HEAD `/v1/models` | vendor ~600 ms vs OpenRouter ~90 ms → rozgrzewać **prawdziwą mini-decyzją**, nie HEAD | j.w. |
| **Rust `uc-jev`, vendor, 5 pytań** (2026-09-20) | N=12: **274.5 ms** (1 469 tok); N=30: **287.9**; N=60: **297.5**; floor 258–275; rozgrzewka zimnego połączenia 708–754 ms; hedging 400 ms nie wygrał ani razu (p95 324–417) | `bench/R2-jev.md` |
| **Rust `uc-jev`, vendor vs OpenRouter w jednym oknie** (2026-09-20 22:02–22:07, 10 wywołań/rozmiar) | vendor N=12/30/60/120: **302 / 287 / 286 / 328 ms**; OpenRouter: **294 / 314 / 323 / 367 ms** → OpenRouter +27…+39 ms p50 od N=30 (narzut rośnie z ładunkiem), p95 +54…+85 ms, floor identyczny 267 ms; hedge 350 ms: 5 wystrzelonych, 0 wygranych (max w 110 wywołaniach 425 ms < 350 + floor) | `bench/R2-jev.md` seria B |
| Koszt kroku CU | ~$0.0001–0.0003 (1.7k–6k tok); 1 000 kroków ≈ $0.04–0.34 | j.w. + JevUse §8.5 |
| Mały LLM na trywialnym pytaniu | gemini-2.5-flash-lite: $0.0000057 vs Jev $0.0000133 — **Jev nie wygrywa ceną pojedynczego pytania**, wygrywa strukturą, rozkładami i fan-outem | jevskill E6 |

### 3.2 Trafność / kalibracja

| Pomiar | Wartość | Źródło |
|---|---|---|
| Powtarzalność `choice` | 90.8% surowa → **99.2% z progiem top-p ≥ 0.60** (74.2% decyzji auto) | cookbook consistency_choice |
| Guard `noul` (syntetyczne logi) | 100% na 49/80 decydujących; pasmo 0–20% → 0.00 realnie, 80–100% → 1.00 | jevskill E5 |
| Jev solo vs Haiku 4.5 (phishing, 2 000 maili) | **62.6% vs 81.3%**; 5 sygnałów Jev + regresja logistyczna **95.1%**; regex 91.8% | anisselbd/jev-phishing-bench |
| Kaskada 182 opcji → top-3 re-rank | błędne 16.8% → 7.3%, zbędne 9.8% → 4.0% | cookbook skill_suggestion |
| Beam K=3 vs greedy (hierarchia) | 4/4 vs 2/4 | cookbook hierarchical_classification |
| Pytanie „czy ekran się zmienił po akcji" (`stuck`) | **0.42–0.60 na ekranie, który ewidentnie się zmienił** — Jev nie porównuje stanów | cu_bench 2026-09-20 |

Wniosek z 3.2: **Jev jako generator wielu tanich sygnałów + kombinacja w kodzie**, nie wyrocznia z jednym pytaniem.

### 3.3 Istniejące Computer Use na Jev — co zmierzyli

| Projekt | Platforma / stack | Krok | Liczby |
|---|---|---|---|
| browser-use/**jev-ultrafast** | Chrome via CDP | `op` + `target` w jednym RT; mały LLM (`inception/mercury-2.5`) tylko dla TYPE_TEXT; settle 50 ms / 2 klatki, combobox ≤200 ms | Google Flights **7.07 s** (z 9.45 s), CDP calls 1 092 → 101; Wikipedia 2.8 s; **$0.0039/zadanie**; „`DONE` still requires independent outcome verification" |
| awlevin/**typesafe-computer-use** | macOS, Python; OCR (kafle 256 px, re-OCR gdy >60% kafli zmienionych) + AX | 3 Choice: `kind` (11 opcji: click_item, press_offscreen, use_browser, type_text, type_email, press_enter, press_escape, scroll_down, scroll_up, wait, done/none), `item`, `site`; gate conf ≥ 0.4; Noul „sensible value" po wpisaniu (<0.5 → czyść pole); writer LLM = claude-haiku-4-5 | **krok 0.95 s** = capture 0.31 + ocr 0.31 + ax 0.06 + **decide 0.21** + act 0.05; $0.0002/decyzja (155× taniej niż Opus); pokrycie AX: Finder 100%, Chrome 88%, Slack 85%, Notion 68%, **Spotify 0%** |
| kofanlabs/**typesafe-computer-use-windows** | Windows, Python 3.12 + Node 20; PrintWindow + Windows.Media.Ocr + UIA; MCP host daje tekst i weryfikuje ekran końcowy | jak wyżej + handoff `needs_host` | grid form **5.5 s**; „2.243 s UI interaction after host reply"; **PrintWindow pada na GPU-rendered**; tylko główny monitor; klucz w DPAPI |
| paulsmith/**computer-use-jev** | macOS, Go + Swift worker AX | batch: action, target token (`a1/w2/e5`), goal satisfied, text needed; „Closed sets stay closed" | brak liczb; „Open" = tylko aktywacja działającej apki |
| moritzkremb/**jev-voice-browser** | Node, Playwright; **STT = Web Speech API (Google)** | 9–11 pytań na **każdy częściowy transkrypt**: `intent` (13), `target`, `site`, `complete`, `is_command`, `destructive`, `scroll_amount`, `text_span`, `url_span`; ≤100 elementów; overlay z numerami przy niejednoznaczności; „confirm/cancel" głosem dla destrukcyjnych | Jev ~330 ms śr., p50 ~300; debounce 200 ms; **ostatnie słowo → decyzja ~300 ms**; pierwszy request ~700 ms (TLS) |
| jkudish/jev-browser | Playwright, DOM ≤240 el. | action + goal noul + stuck noul | Wikipedia ~4 s, $0.0016; stop: goal > 0.85 / stuck > 0.85 |
| NousResearch/hermes-agent #113850 | propozycja | `ACTION / TARGET / NEEDS_VISION / NEEDS_GENERATION / DONE`; hierarchia: reguły → reranker lokalny → mały model → Jev | brak liczb |

**Luka, której nikt nie zamknął** (to jest miejsce na „najszybszy"): (a) UIA-first bez OCR na gorącej ścieżce, (b) diff stanu w kodzie, (c) hedged requests, (d) cache makr, (e) spekulacyjny plan, (f) **głos jako źródło tekstu** (zamiast LLM-writera), (g) jeden natywny, przenośny `.exe` zamiast Python+Node.

---

## 4. Jaggedness — 11 udokumentowanych słabości → 11 reguł dla CU

Strona `model-jaggedness/jev-1.13.md` (przegląd producenta 2026-09-17). Cytat → reguła w naszym kodzie:

| # | Słabość (cytat) | Reguła w Jev-UltraCuse |
|---|---|---|
| 1 | Literal reading: „answers the question you wrote, not the one you meant" | warunki brzegowe wprost w `criteria`; jedno pytanie = jedna idea |
| 2 | „Jev is not a calculator", „does not count reliably" | liczenie elementów, kroków, znaków — **kod** |
| 3 | Numeric representations (hex, „near each other") słabo | współrzędne, odległości, rozmiary → **bucketuj w kodzie** („obok kursora", „na górze"), nie surowe piksele w pytaniu |
| 4 | Score „weak in numerical calibration" | `score` tylko do progów/etapów, nie do interpolacji |
| 5 | Date/time „reads dates as text" | daty w state parsuj i etykietuj w kodzie |
| 6 | Indirection, podwójne negacje | pytania proste, cel nazwany ścieżką |
| 7 | „Unrelated detail acts as a distractor" | **REDUCE w kodzie** przed wysłaniem: widoczne ∧ enabled ∧ rola interaktywna, ≤60 kandydatów, nazwy ≤48 znaków |
| 8 | „does not treat [data] as hostile by default" | **UI może zawierać prompt-injection** (tytuł okna, tekst strony) — guardy destrukcyjne w kodzie, nigdy „bo Jev powiedział" |
| 9 | Contradictory instructions/criteria | lint spójności bundle'a pytań w testach |
| 10 | **P(x) ≠ 1 − P(¬x)** nie gwarantowane | nie licz na tożsamości między pytaniami; jeden fakt = jedno pytanie |
| 11 | Generation: „not trained to generate text", „very slow" | tekst do pól: **STT (dyktowanie)** → dane zadania → mały LLM; nigdy Jev |

Dodatkowo z `/concepts/state.md`: „lower accuracy for non-English content, particularly CJK" — **polskie kryteria wymagają pomiaru PL vs EN** (JevUse E2E z polskimi kryteriami dało `done=0.94` i PASS; to jeden run, nie dowód). Zadanie pomiarowe w planie.

---

## 5. Oficjalne wzorce → nasza pętla

| Wzorzec (docs) | Cytat | Zastosowanie w CU |
|---|---|---|
| **Speculative fan-out** (`/patterns/fan-out`) | „Send many questions in a single call, including speculative ones, and let your code decide what's relevant." | bundle kroku: `target`, `op`, `goal_reached`, `needs_text`, `is_destructive`, `progress`, `recovery` — wszystkie naraz, kod czyta tylko właściwe |
| **Confidence-gated routing** (`/patterns/confidence-routing`) | „Below 0.6 confidence on any action, route to a human"; high-stakes > 0.85 auto, 0.6–0.85 → potwierdzenie; „each action type has its own threshold based on the consequences" | progi per typ akcji: klik 0.6, wpisanie 0.6, klawisz systemowy 0.75, akcja z listy nieodwracalnych 0.85 + **zawsze** potwierdzenie głosowe |
| **Function calling** (cookbook) | argumenty = Choice/Set/Flag; wolny tekst/liczby/daty **nie dostają pytania** (domyślne); confidence = **najsłabsze** pytanie; „Write each question about the idea rather than the words a user might pick" | słownik akcji jako zamknięte argumenty; parametry liczbowe (ile scrolla) jako `score` z etykietami (`little/page/end`), nie liczby |
| **Skill suggestion** (kaskada) | 182 opcje w jednym Choice + 3 Nouly bramkujące → re-rank top-3 | okna > 60 kandydatów: region → element (dwa wywołania, drugie tylko przy niskiej pewności) |
| **Self-consistency: choices** | próg top-p 0.60 → 99.2% powtarzalności | akcje nieodwracalne: 3 sformułowania w jednym fan-oucie, zgoda ≥ 2/3 |
| **Hierarchical classification** | beam K=3 4/4 vs greedy 2/4 | beam nad kaskadą region→element przy conf < 0.6 |
| **Re-ranking** (BM25 + Noul per kandydat) | top-1 5% → 18% | dopasowanie wypowiedzi użytkownika do nazw elementów: lokalny fuzzy/BM25 daje top-10, Jev wybiera |
| **State-responsive decisions** (`/agent-skill`) | „retain goals while fresh judgments guide bounded next steps"; „Put the constants (questions and thresholds) in a single place so they're easy to review" | `goal` stały w state, świeży snapshot co krok; **jeden plik stałych** (`decide/consts.rs`) z pytaniami i progami |

---

## 6. Czego Jev NIE zrobi w CU i kto to robi u nas

| Potrzeba | Jev? | Rozwiązanie |
|---|---|---|
| Treść do wpisania | nie | **dyktowanie (STT)** jako główne źródło — zgodne z celem dostępności; dane zadania (cytaty w celu); mały LLM tylko gdy użytkownik prosi „napisz…" |
| „Czy ekran się zmienił" / „czy akcja zadziałała" | nie (0.42–0.60) | hash znormalizowanego drzewa UIA przed/po; zdarzenia UIA `StructureChanged`/`PropertyChanged` |
| Plan wieloetapowy | nie | LLM na starcie zadania (3–5 kroków), Jev tylko weryfikuje `plan[i]` (Noul) i wybiera cel |
| Liczenie, daty, geometria | nie | kod |
| Elementy bez UIA (canvas, gry, Spotify, RDP) | nie | OCR lokalny **asynchronicznie** (wynik do następnego kroku), grounding VLM jako ostatnia deska |
| Nieznany dialog / CAPTCHA | nie | eskalacja do człowieka (głosowo) — CAPTCHA nigdy automatycznie |
| Autoryzacja akcji | **nigdy** | Jev doradza (`is_destructive`), kod decyduje: lista + próg + potwierdzenie |

---

## 7. Budżet kroku — cel dla „najszybszego"

| Etap | Cel | Jak (dźwignia) |
|---|---|---|
| Percepcja | 5–60 ms **tylko z cache drzewa + zdarzeniami UIA**; pełny skan zmierzony (R1, `bench/R1-uia.md`): Chrome 108 ms, Notatnik Win11 380 ms, Electron 464 ms — identycznie w Rust i Python | UIA tylko aktywne okno, **jedno** `FindAllBuildCache` z OR-warunkiem + `CacheRequest` (99.9 % czasu po stronie providera a11y — język klienta nie ma znaczenia); invalidacja cache zdarzeniami `StructureChanged`/`PropertyChanged`; Win32 (kursor/okna) w µs; **bez OCR na gorącej ścieżce** |
| Redukcja + hash | ≤ 2 ms | kod |
| Cache makr | 0 ms przy trafieniu | klucz = (cel znormalizowany, hash drzewa znormalizowanego) → decyzja |
| **Decyzja Jev** | **~290–330 ms** (floor: sieć ~60 ms + inferencja ~250 ms) | 1 wywołanie; ciepłe h2; końcówka vendora (−30…−60 ms); **hedging** tylko jako straż przed zawieszeniem: drugi identyczny POST po ~600 ms (2×p50) — hedge z progu 350–400 ms wygrał 0/8 razy, bo wygrywa dopiero przy stallu > próg + floor ≈ 620 ms, a max z 110 wywołań to 425 ms; timeout 1.5 s |
| Walidacja | < 1 ms | element istnieje, bbox na ekranie, nie zasłonięty, `op` dozwolony dla roli, próg per typ |
| Akcja | < 1–5 ms | UIA `Invoke`/`SetValue` gdy dostępne; inaczej **jeden `SendInput`** z całą sekwencją (ruch+klik, chord, tekst) |
| Settle | 0–200 ms | czekaj na zdarzenie UIA lub zmianę hasha; cap 200 ms (jev-ultrafast: 50 ms / 2 klatki; combobox 200 ms) |
| **Krok** | **~350–500 ms** | vs 0.95 s (typesafe-computer-use), 567 ms p50 (arXiv 2607.28399 „Correct but Late") |

Liczba kroków jest ważniejsza niż ms w kroku: OSWorld-Human (arXiv 2506.16042) — agenci robią **2.7–4.3× więcej kroków niż człowiek**, planowanie to 53–75% czasu. Stąd: spekulacyjny plan, makra, i **głos jako skrót** („zapisz jako raport.docx na pulpicie" = jeden krok zamiast pięciu kliknięć, gdy UIA daje `SetValue`).

---

## 8. Ryzyka i otwarte pytania

1. **Alpha API / jeden provider** — brak failoveru na inny model decyzyjny; wymagany tryb degradacji (LLM lub tylko dyktowanie).
2. **Rate limity dynamiczne** — 1 200 req/min to ~20 kroków/s; pętla 2–3 kroki/s mieści się z zapasem, ale honorować `retry-after`.
3. **Klucz vendora** — waitlista; na tej maszynie są oba klucze w `HKCU\Environment`: `JEV_API_KEY` (vendor, domyślny) i `OPEN_ROUTER_API_KEY` (OpenRouter, odświeżony 2026-09-20; stary `JEVUSE_API_KEY` wygasł — 401). Klient czyta rejestr, nie tylko env procesu (zmienne ustawione po starcie terminala nie dziedziczą się); nazwy dla OpenRouter w kolejności: `OPENROUTER_API_KEY`, `OPEN_ROUTER_API_KEY`, `JEVUSE_API_KEY`. Vendor-first; OpenRouter tylko na żądanie (`--provider openrouter`, `UC_PROVIDER=openrouter`) albo gdy brak klucza vendora.
4. **Polskie kryteria** — docs ostrzegają o niższej dokładności poza angielskim; potrzebny test A/B PL vs EN na tych samych ekranach.
5. **Kalibracja progów** — `confidence` mierzy koncentrację rozkładu, nie poprawność; progi 0.6/0.85 to punkt startu, mierzyć na własnych parach decyzja→wynik.
6. **Pokrycie UIA** — Electron/canvas/Spotify: 0% widoczności → OCR/VLM ≥ 600 ms; mierzyć odsetek kroków na fallbacku per aplikacja.
7. **Injection z UI** — tekst z okien trafia do state; nie może sterować guardami.
8. **Alias `jev-latest`** przesuwa się — pinować `jev-1.13.0`, logować versioned id z odpowiedzi.

---

## 9. Wnioski projektowe (ograniczenia dla architektury)

1. Rdzeń to **pętla tekstowa**: UIA → JSON state ≤ 2k tokenów → 1 POST → walidacja → SendInput. Zero obrazów w gorącej ścieżce.
2. **Jeden plik stałych** z pytaniami i progami; pytania kompilowane raz do bajtów; klucze `choice` = `e0…eN` + `none`.
3. **Diff, liczenie, guardy — kod.** Jev doradza, kod decyduje.
4. **STT jest częścią pętli decyzyjnej**, nie tylko wejściem: częściowy transkrypt → fan-out (`is_command`, `intent`, `target`, `complete`, `destructive`) jak w jev-voice-browser, ~300 ms od ostatniego słowa; treść dyktowana ląduje w polach bez LLM.
5. **Klient HTTP na gorąco**: h2 keep-alive, rozgrzewka mini-decyzją, hedging, timeout 1.5 s, ledger poza gorącą ścieżką, vendor-first z fallbackiem na OpenRouter.
6. **Każda liczba w README z benchmarku w repo** (zasada odziedziczona z jevskill/AGENTS.md).

---

## Źródła

- docs: <https://docs.typesafe.ai/llms.txt> · `/api.md` · `/models.md` · `/concepts/state.md` · `/patterns/fan-out.md` · `/patterns/confidence-routing.md` · `/model-jaggedness/jev-1.13.md` · `/agent-skill.md` · `/sdk.md`
- cookbooki: `parallel_questions` · `function_calling` · `skill_suggestion` · `consistency_choice_cookbook` · `hierarchical_classification` · `rerank_typesafe`
- OpenRouter: <https://openrouter.ai/typesafe/jev-1.13> · Decisions API reference
- pomiary lokalne: `D:\code\JevUse\bench\e1.jsonl`, `e2.jsonl`, `README.md`; `D:\code\jevskill\bench\cu_bench.py`, `docs/research-2026-09-20-jev-cu.md`, `skills/jev/references/benchmarks.md`
- CU na Jev: <https://github.com/browser-use/jev-ultrafast> · <https://github.com/awlevin/typesafe-computer-use> · <https://github.com/kofanlabs/typesafe-computer-use-windows> · <https://github.com/paulsmith/computer-use-jev> · <https://github.com/moritzkremb/jev-voice-browser> · <https://github.com/jkudish/jev-browser> · <https://github.com/NousResearch/hermes-agent/issues/113850> · <https://github.com/cobanov/awesome-jev>
- benchmarki: <https://github.com/anisselbd/jev-phishing-bench> · <https://jevbench.xyz/methodology>
- literatura: arXiv 2506.16042 (OSWorld-Human) · 2607.28399 (Correct but Late) · 2609.03236 (Speculative Macro Commit) · 2604.24039 (AgenticCache)
