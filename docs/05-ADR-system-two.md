# ADR-003: System Two — LLM z OpenRouter „między wierszami” pętli Jev

Status: **przyjęty** 2026-09-21 (życzenie użytkownika 2026-09-20: „równolegle, w trakcie tasków, nie przerywając ciągu — LLM z OpenRouter z wyborem konkretnego modelu przez wygodny modal"). Zastępuje szkic z `docs/03-architektura.md` §9 (tam: „wywoływany rzadko, zwraca jeden wiersz planu"). Kod: `crates/uc-two`, integracja w `crates/uc-loop` (`RunOpts::two`, `policy::from_advice`, `apply_reply`), CLI `--two [MODEL]`, okno: Ustawienia → System Two.

## Kontekst

Jev (System One) decyduje w ~300 ms: **który** element i **która** operacja. Nie planuje i nie pisze. Trzy sytuacje, w których pętla MVP-1 kończyła się przedwcześnie (`bench/R4-mvp-run.md`):

1. cel wieloetapowy („stwórz na pulpicie plik") — pytanie `goal_reached` o cały cel jest za szerokie; Jev widzi tylko bieżący ekran;
2. `Uncertain` dwa kroki z rzędu — pętla poddaje się, choć człowiek widziałby oczywisty następny ruch;
3. `needs_text` bez dyktowanego tekstu — pętla nie ma czego wpisać.

## Decyzja

Model czatu z OpenRouter (`POST /api/v1/chat/completions`) dostaje **ten sam zredukowany stan**, który widzi Jev (scena + elementy `e<i>`, w tym pop-upy), i odpowiada **jednym obiektem JSON**: `plan` (podcele), `subgoal` (przeformułowanie), `next` (konkretny krok), `text`, `note`. Trzy wejścia, jeden wątek, jeden kanał:

| kiedy | co idzie do modelu | co pętla robi z odpowiedzią | czy pętla czeka |
|---|---|---|---|
| krok 1 | `task=plan`, cel, stan | plan przyjęty **raz** (nigdy nie podmieniany); od tej chwili `goal` w stanie Jev = bieżący podcel, `plan.{overall,steps,current}` obok; `Done` na podcelu = następny podcel z wyzerowanymi licznikami, `Done` na ostatnim = koniec | **nie** — odpowiedź zbierana `try_recv` przy kolejnych krokach |
| `Uncertain` (po rundzie zawężenia) | `task=rescue`, stan, rozkład Jev jako prior, powód | `next` → `policy::from_advice` (te same bramki co decyzja Jev); `subgoal` podmienia bieżący podcel; `text` uzupełnia dyktowanie | **tak**, do `TWO_WAIT_MS` — krok i tak skończyłby się pustym `Uncertain`; kill-switch i Stop odpytywane co 100 ms |
| `NeedsText` bez tekstu | `task=rescue`, `need_text=true` | `text` → `dictated`, decyzja Jev oceniona ponownie | jak wyżej |

Budżet: `TWO_MAX_CALLS` = 3 na przebieg (plan + 2 ratunki). Progi tylko w `uc-loop::consts` (`TWO_MAX_CALLS`, `TWO_WAIT_MS`, `TWO_TIMEOUT_MS`, `TWO_DEFAULT_MODEL`).

**Bramki są w kodzie, nie w modelu.** Propozycja LLM nie ma skalibrowanej pewności, więc `from_advice` traktuje ją ostrzej niż Jev: element musi istnieć w stanie i być `enabled`; klawisz z listy `q::KEYS`; wszystko nieodwracalne (lista nazw, `delete`) wymaga jawnego `--allow-irreversible` (bez progu 0,85 — bo nie ma czego mierzyć); `done` z ratunku tylko przesuwa podcel, nie kończy przebiegu bez potwierdzenia Jev. Sprawdzenie fokusu tuż przed `SendInput` obowiązuje tak samo.

**Model wybiera użytkownik.** Okno → Ustawienia → System Two: lista `GET /api/v1/models` (pobierana w tle, z ceną za Mtok i kontekstem), wyszukiwarka, ulubione, gwiazdka; wybór i ulubione zapisywane w `ultracuse.settings.json` obok exe (portable). Klucz **nigdy** nie ląduje w pliku ustawień — tylko `OPENROUTER_API_KEY` / `OPEN_ROUTER_API_KEY` ze środowiska użytkownika (proces albo `HKCU\Environment`), ta sama ścieżka co w `uc-jev`. Domyślny model: `google/gemini-2.5-flash-lite` (tani, szybki; użytkownik używa go już w innych projektach).

Prywatność jak dla Jev (§10 architektury): do modelu idzie tylko zredukowany tekst aktywnego okna, nigdy zrzuty ekranu; koszt i tokeny z `usage.include=true` trafiają do ledgera (`StepRecord.two`, `RunSummary.two_*`).

## Odrzucone

- **LLM jako główny decydent, Jev jako weryfikator** — odwraca proporcje kosztów i latencji (LLM 2–8 s/krok vs Jev 0,3 s); Jev ma rozkłady, na których działają bramki; LLM nie.
- **Blokowanie pętli na plan** — plan przychodzi po 2–5 s, pierwszy krok Jev po 0,5 s; czekanie na plan kosztowałoby więcej niż jeden zbędny krok, a przy celach jednoetapowych plan jest zbędny.
- **Podmiana planu przy każdym ratunku** — model widzący jeden ekran przepisywałby cały plan; wolimy lokalną korektę (`subgoal`).
- **`response_format: json_schema`** — nie każdy model na OpenRouter go obsługuje; `json_object` + tolerancyjny parser (`parse_advice`: płoty ```` ``` ````, proza wokół obiektu) + jedna powtórka bez `response_format` po 400/404.

## Konsekwencje i pomiar

- Bez klucza OpenRouter aplikacja działa jak dotąd (Jev + dyktowanie); System Two jest opt-in (checkbox / `--two`).
- Kryterium z TASKS 3.7: średnia liczba wywołań Jev na zadanie −30 % przy celach wieloetapowych **bez** spadku odsetka sukcesu — do zmierzenia w `bench/R5-system-two.md` na tych samych zadaniach co R4 (Notatnik: wpisz + zapisz; pulpit: nowy plik), z planem i bez.
- Każde uruchomienie System Two to płatne wywołanie (poza modelami `:free`); koszt wypisany w podsumowaniu i w ledgerze.
