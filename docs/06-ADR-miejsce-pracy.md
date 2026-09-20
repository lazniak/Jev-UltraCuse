# ADR-004: Miejsce pracy zamiast okna docelowego — iteracyjne poszerzanie kontekstu przy niepewności

Status: **przyjęty** 2026-09-21 (życzenie użytkownika: „nie chcę wskazywać żadnych okien; model ma pracować zawsze z miejsca, w którym startujemy task; ma samodzielnie minimalizować okna, jeżeli uzna, że nie ma wśród nich żadnego z potrzebnych przycisków"). Zastępuje blokadę na pid z MVP-1 (`docs/03-architektura.md` §13, przegląd PR #1) i wybór okna docelowego z okna (PR #2, #5). Kod: `crates/uc-loop` (`Runner::run`, `survey`, `policy::Action::{Switch, ShowDesktop}`, `consts::WIDEN_*`), `crates/uc-win32` (`show_desktop`, `desktop_hwnd`, `minimize`), okno (`start_window`).

## Kontekst

MVP-1 blokował przebieg na procesie okna docelowego: inne okno na wierzchu = `FocusLost`, koniec. Bezpiecznie, ale ślepo: cel „stwórz na pulpicie plik" uruchomiony z okna czatu kończył się dwiema niepewnymi decyzjami (ledger 2026-09-21), bo właściwe miejsce — pulpit — było zasłonięte, a pętla nie miała prawa go odsłonić. Użytkownik nie chce wskazywać okien; chce, żeby pętla **znalazła** miejsce.

## Decyzja

**Miejsce** (`place`) zamiast celu: pętla rusza z okna, w którym użytkownik był przed chwilą (okno: ostatnie aktywne okno nie-nasze, jeśli wciąż istnieje, inaczej to, co jest na wierzchu po zminimalizowaniu okna aplikacji; CLI: okno na wierzchu po odliczaniu lub `--hwnd`), i podąża za skutkami **własnych** akcji: dialogi i inne okna tego samego procesu przejmowane po cichu, okno otwarte lub zamknięte kliknięciem — przejęte („focus moved to…" w `last`), przełączenie i pulpit — jawne akcje.

**Drabina poszerzania** — jeden szczebel na każdy niepewny krok, zero po każdej wykonanej akcji:

| szczebel | co się zmienia | koszt |
|---|---|---|
| 0 | okno: kontrolki interaktywne (≤ 60), pop-upy procesu | 1 wywołanie Jev |
| `WIDEN_CONTEXT` | + etykiety i tekst statyczny (`include_context`), ≤ 120 kandydatów | 1 wywołanie Jev, większy stan |
| `WIDEN_SURVEY` | **przegląd okien**: tytuły i exe wszystkich otwartych okien (bez UIA), pytanie `place`: które okno, `desktop` (zminimalizować, co zasłania) albo `none`; odpowiedź = akcja `Switch` / `ShowDesktop` albo „zostań" | 1 wywołanie Jev, 2.3–2.5 k tokenów przy 24 oknach (R6), bez UIA |
| `WIDEN_TWO` | ratunek System Two (ADR-003), tylko gdy włączony | 1 wywołanie LLM |
| dalej | `Outcome::Uncertain` | — |

`target = none` („żaden z wymienionych elementów nie przybliża celu") przeskakuje od razu do przeglądu okien — właściwe miejsce jest prawdopodobnie gdzie indziej.

**Przemieszczenie** (fokus zmienił proces bez akcji pętli): żadnej decyzji o elemencie, żadnego wstrzyknięcia w to okno — tylko przegląd okien: „to jest właściwe miejsce" (przejmij), wróć (`Switch`), pulpit. `DISPLACED_STRIKES` = 3 kolejne przemieszczenia → `FocusLost` (użytkownik wciąż zabiera fokus — pętla nie walczy).

**Pulpit** to nie okno: `Program Manager` nigdy nie jest celem `Switch` (inne okna go zasłaniają — klik w ikonę trafiłby w nie). `ShowDesktop` minimalizuje okna z wierzchu po kolei (`ShowWindow`, bez wstrzykiwania, do `SHOW_DESKTOP_MAX` = 8), aż na wierzchu jest `Progman`/`WorkerW`, i aktywuje okno z ikonami (`desktop_hwnd`: `SHELLDLL_DefView` pod `Progman` albo `WorkerW`). Odwracalne z paska zadań. Pasek zadań na wierzchu to **nie** pulpit (jego przyciski byłyby skanowane i klikane); okno, które nie daje się zminimalizować (podniesione uprawnienia, topmost) = brak postępu = stop. Na pulpicie odpowiedź `desktop` znaczy „zostań”, nie kolejne `ShowDesktop`.

## Co zostaje z bezpieczeństwa

- Wstrzyknięcie tylko w okno, które ten krok skanował: `focus_still_ours(hwnd)` porównuje **uchwyt** okna na wierzchu z uchwytem skanu tuż przed `SendInput` (silniej niż dawny pid). Zmiana w międzyczasie = krok bez akcji, następny krok widzi, dokąd poszedł fokus.
- `Switch` i `ShowDesktop` to `SetForegroundWindow`/`ShowWindow` — nie wejście; obie akcje przechodzą przez tryb podglądu, kill-switch i Stop jak każda inna.
- Lista nieodwracalnych, `is_destructive`, `--allow-irreversible`, bramki System Two — bez zmian.
- Tylko wejście (`click`/`type`/`key`) i akcje okienne mogą przenieść fokus: nowe okno na wierzchu po `wait` albo `scroll` to przemieszczenie przez użytkownika, nie skutek pętli.
- Odmowa menedżera okien (`SetForegroundWindow`, `ShowWindow` przy blokadzie fokusu, oknie podniesionym lub zamkniętym) = krok bez efektu, nie koniec przebiegu; szczebel zostaje, więc drabina idzie dalej.
- Okno może być celem `Switch` najwyżej `SWITCH_MAX_VISITS` = 2 razy w przebiegu: wycieczka A → B → A (skopiuj tam, wklej tu) przechodzi, ping-pong kończy się na kolejnym szczeblu, nie na budżecie kroków.
- Własne okno aplikacji na wierzchu nigdy nie jest miejscem pracy (AccessKit wystawiłby Jevowi nasz Start/Stop): pętla czeka (nie liczy kroków), `OWN_WINDOW_STRIKES` = 10 × 200 ms → `FocusLost`. Pulpit liczy się do limitu odwiedzin jak okno.
- Krok przeglądu nie skanuje UIA — do dostawcy idą **tytuły i exe wszystkich otwartych okien** (≤ 24), więcej niż dawny tytuł jednego okna; bez elementów, bez treści. To jedyny nowy wyciek informacji tej decyzji.
- Tryb podglądu w oknie aplikacji nie dotyka okien użytkownika: bez `show_desktop` przed startem, `Switch`/`ShowDesktop` tylko pokazane.
- Ledger: `StepRecord.widen` (szczebel kroku), `StepRecord.survey` (top-3 przeglądu z tytułami), `last.effect` z przejęciami okien, `StepRecord.minimized` po `ShowDesktop`, `StepRecord.refused` przy odmowie menedżera okien. Ledgery (`runs/`, poza gitem) zawierają odtąd tytuły wszystkich otwartych okien z kroku przeglądu — kopie do `bench/` przeglądać przed commitem.

## Odrzucone

- **Win+D** zamiast minimalizowania po kolei — globalne, przełącza też okna, których pętla nie widziała, i nie zostawia śladu, co zminimalizowano.
- **Przegląd okien przez UIA każdego okna** — 100–460 ms na okno (R1); tytuł + exe wystarcza Jev do wyboru miejsca, a właściwe okno i tak jest skanowane w następnym kroku.
- **Kontynuacja po przemieszczeniu bez pytania** — pętla wpisywałaby tekst w okno, do którego użytkownik właśnie przeszedł (incydent z Chrome przy MVP-1).
- **Stały wybór okna w GUI** — użytkownik nie chce wskazywać; przegląd okien naprawia zły start (dwa przebiegi na oknie czatu z 2026-09-21).

## Pomiar

`bench/R6-widen.md` (2026-09-21, podgląd, start z Notatnika): trzy szczeble potwierdzone na żywo — kontekst, `Switch` na FileZilla (1.00), `desktop` (0.70); 2 kroki i 2 wywołania Jev na przebieg, $0.0002–0.0004. Do zrobienia: to samo z `--act`, przemieszczenie przez użytkownika i `FocusLost`, szczebel System Two.
