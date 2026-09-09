# M-romance — All Romance languages, UI + deck (plan)

Planned 2026-09-08. NOT started (extends the shipped M-i18n, which has en + es).
Owner: support ALL Romance languages; when the user changes language, the
FRONT-END switches dynamically AND the LLM deck switches to the target language.

## Target languages
Major Romance languages (superset of the current en/es):
- **es** Spanish (shipped) · **pt** Portuguese (huge — Brazil) · **fr** French ·
  **it** Italian · **ro** Romanian · **ca** Catalan · **gl** Galician.
- Stretch: Occitan (oc), Romansh (rm), Sardinian, Aromanian — add later; start
  with the 6 above + the existing en/es. Keep `en` as the source/fallback.

## What already exists (reuse, don't rebuild)
- `crates/app/src/i18n.rs`: `Lang` enum (En, Es), `t(key)` keyed lookup with
  English fallback, en/es catalogs, a completeness test (`every_english_key_has_spanish`).
- `language <code>` app-verb + Theme ▸ Language menu radios + ui.json persistence
  + LIVE front-end switch (already dynamic — no relaunch).
- The deck prompt (compact catalog) is in English today.

## Part A — Front-end: all Romance languages
1. Extend `Lang` → { En, Es, Pt, Fr, It, Ro, Ca, Gl } (serde codes en/es/pt/fr/it/ro/ca/gl;
   accept regional variants like `es-CO`, `pt-BR` → base language).
2. Add a catalog per language covering EVERY `t()` key. Seed via machine
   translation, then flag for native review (quality caveat — mark auto-translated
   strings so a native speaker can correct; don't claim perfect translations).
   CAD terminology per language (layer: capa/camada/calque/livello/strat/capa;
   dimension: cota/cotação/cote/quota/cotă; sheet, lot, setback, etc.).
3. **Completeness test generalized**: every language's catalog has every English
   key (no missing keys, no orphans) — one test parametrized over all `Lang`.
   This is the anti-drift guard as keys grow.
4. Language picker (menu + `language <code>` verb + onboarding) lists all; live
   switch already works. Font check: verify the UI font covers Latin-extended
   diacritics (á é í ó ú ñ ç ã õ â ê î ô û à è ì ò ù ë ï ü ș ț ă î â) — pick/confirm
   a font with full coverage; test a diacritic-heavy label renders.
5. **Locale-aware formatting**: decimal comma vs point, thousands sep, unit
   spacing per locale (es/pt/fr/it use comma decimal; en uses point). A
   `format_number(value, lang)` helper used where the UI shows numbers.

## Part B — Deck (LLM) follows the language (the new bit)
The KEY invariant: **command tokens stay canonical English.** The parser + GBNF
grammar only accept `box`/`geodesic`/`lotsubdivide`/etc. — those are NOT
translated. Only the model's PROSE (explanations, questions, plan text, critique)
and the app-generated deck strings localize.
1. **Prompt directive keyed to the UI language**: the deck system prompt gains a
   line "Respond in <language name>. Command tokens stay exactly as listed (do not
   translate them)." So a French user gets French explanations but the model still
   emits `geodesic 3 5 dome`. LLMs are already multilingual → they understand a
   prompt typed in pt/fr/it/ro fine; this just steers the REPLY language + protects
   the command tokens.
2. **Localize app-generated deck strings**: status lines, error messages,
   clarify-question scaffolding, plan checklist labels, the "using euro_latam
   defaults" note, critique framing — route through `t()` so they match the UI
   language. The catalog + completeness test cover them.
3. **Terse/clarify/plan** message forms localize their fixed wrapper text; the
   command payload stays canonical.
4. Small/local models: the compact catalog (English verb names + one-liners) stays
   English (it's the command reference); add a short localized instruction to reply
   in the target language. Test that a non-English UI language still emits valid
   canonical commands (grammar unaffected).
5. Optional: a per-cassette language override (a user could keep the deck in
   English while the UI is Portuguese) — default = follow UI language.

## Tests
- Every language catalog complete vs English (parametrized completeness test).
- Diacritic label renders (font coverage).
- `format_number` per locale (comma vs point).
- Deck prompt contains the "respond in <lang>" directive for a non-en language,
  and command tokens are NOT translated (the catalog/grammar test still green;
  a test that the prompt's command list stays English while the directive names
  the target language).
- Round-trip persistence of each `Lang` in ui.json (incl. regional-variant folding).

## Notes / honest caveats
- Translation QUALITY: machine-seeded translations need native review; ship with a
  clear "community translations welcome / auto-translated, corrections invited"
  stance (ties to the open-source/community strategy). Don't overclaim fluency.
- Command tokens never localize — that's a hard invariant (parser + grammar).
  Aliases (Rhino/AutoCAD skins) are separate + also English-token-based.
- This is high-value for the LatAm/Global-South + Francophone-Africa + Brazil
  markets — pt + fr especially widen reach a lot beyond es.
