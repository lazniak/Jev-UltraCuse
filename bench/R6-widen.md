# R6 — miejsce pracy i drabina poszerzania (ADR-004): przebiegi na żywo w trybie podglądu

Data: 2026-09-21 00:55–01:07. Maszyna: i9-7960X, 128 GB, Windows 11 26100, DPI per-monitor v2 (jak R1). Końcówka vendora `api.typesafe.ai`, model `jev-1.13`, klient `uc-jev`. Skrypt: `bench/r6_widen.ps1` — bramka bezczynności użytkownika ≥ 45 s (`GetLastInputInfo`), start z okna **Notatnika** wysuniętego na wierzch (celowo niewłaściwe miejsce), tryb podglądu: nic nie wstrzyknięte, `Switch`/`ShowDesktop` pokazane, nie wykonane, `--max-steps 4`. Surowe ledgery (kopie z `runs/`): `r6/01-desktop-file-from-notepad.jsonl`, `r6/02-filezilla-from-notepad.jsonl`, `r6/03-recycle-bin-from-notepad.jsonl`. Kod: gałąź PR #6 **przed** poprawkami z przeglądu krzyżowego (krok przeglądu skanował jeszcze UIA — stąd `scan` w kroku 2; po poprawce krok przeglądu nie skanuje).

| # | cel (start: Notatnik) | krok 1 | krok 2 | wynik | Jev | koszt |
|---|---|---|---|---|---|---|
| 01 | „Stwórz na pulpicie nowy plik tekstowy o nazwie r6test.txt" | `Plik` 0.80 (gap 0.65), op click 0.52 → niepewny | szczebel 1 (kontekst, 195→54 kandydatów): `Plik` 0.77, op click 0.60 → **click „Plik"** | Preview, 2 kroki, 1466 ms | 2 | $0.00038 |
| 02 | „Połącz się z serwerem FTP w programie FileZilla" | `target none` 0.54, op click 0.59 → niepewny | szczebel 2 (przegląd okien): FileZilla **1.00** → **switch „FileZilla"** | Preview, 2 kroki, 719 ms | 2 | $0.00018 |
| 03 | „Otwórz Kosz, który jest na pulpicie" | `target none` 0.46, op key 0.44 → niepewny | szczebel 2 (przegląd okien): `desktop` **0.70**, w6 0.24, none 0.04 → **show desktop** | Preview, 2 kroki, 1461 ms | 2 | $0.00026 |

Czasy kroków (ledgery): krok elementowy — scan 78–487 ms, Jev 274–324 ms, 2.0–5.5 k tokenów stanu (5.5 k = szczebel kontekstu, 120 kandydatów); krok przeglądu — Jev 274–279 ms, 2.3–2.5 k tokenów (24 tytuły okien jako kryteria).

## Wnioski

- Trzy szczeble działają jak w ADR-004: kontekst (01), przełączenie na inne okno (02), pulpit przez minimalizowanie (03). `target = none` skacze od razu do przeglądu (02, 03) — bez marnowania kroku na etykiety okna, które nie ma nic do zaoferowania.
- 01 to nie błąd drabiny: w Notatniku „Plik → Zapisz jako → Pulpit" jest legalną drogą do pliku na pulpicie, więc Jev ma prawo zostać. Przegląd okien uruchamia się, gdy okno nie ma pasującego elementu — a nie dlatego, że *my* uważamy, że to złe okno.
- Krok przeglądu nie jest „~200 tokenów", jak zakładał ADR-004 w pierwszej wersji: tytuły 24 okien jako kryteria pytania kosztują tyle co mały krok elementowy. Nadal 1 wywołanie Jev, ~0.28 s, ~$0.0001.
- Wynik negatywny / nie zmierzone: przebieg z `--act` (faktyczne minimalizowanie okien, klik w tło pulpitu, przełączenie), przemieszczenie przez użytkownika i `FocusLost`, szczebel 3 (System Two — płatny model, osobna zgoda), skalowanie DPI i przycięcie tła do obszaru roboczego (poprawki po przeglądzie PR #5 — tylko testy jednostkowe).
