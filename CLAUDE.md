# Jev-UltraCuse — zasady repo

- Język dokumentacji i commitów: polski (kod, identyfikatory, komentarze w kodzie: angielski).
- Decyzje projektowe żyją w `docs/` jako ADR-y; zmiana decyzji = nowy ADR, nie edycja starego.
- **Każda liczba** w README/docs pochodzi z `bench/` (skrypt + JSON + data + maszyna). Wynik negatywny zostaje w repo.
- Gałąź per zadanie `agent/<sesja>/<slug>`; zajęcie zadania = pierwszy commit z wpisem w `TASKS.md`. `main` tylko przez PR, squash, CI zielone.
- Conventional Commits, scope = nazwa crate'a (`feat(uc-uia): …`). Trailer `Co-Authored-By` z modelem, który pisał.
- Przed PR: `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace && cargo build --release`.
- Stałe decyzyjne (pytania, progi, listy nieodwracalnych) tylko w `crates/uc-loop/src/lib.rs::consts` — nigdzie indziej.
- COM (UIA) wyłącznie na dedykowanym wątku STA; żadnych alokacji ani I/O w ścieżce `SendInput`.
- Sekrety: env / `HKCU\Environment`; nigdy w logach (fingerprint SHA-256[:8] co najwyżej), nigdy w repo. Modele STT w `models/` (gitignore).
- Akcje domyślnie w trybie podglądu; `--act` uzbraja; kill-switch Ctrl+Alt+K sprawdzany przed każdą akcją.
- Laboratorium Python: `D:\code\JevUse` (pomiary E1–E6, kształty pytań) — porty do Rust cytują plik źródłowy w docstringu.
- GLM (Z-Code) zawieszony decyzją użytkownika 2026-09-07; kod piszą Claude/Opus wg tabeli z globalnego CLAUDE.md.
