# Flashcard reviews: `.quire/shared/srs/<device-id>.jsonl`

One file per device, so devices never write the same file; it travels with
the vault. Owner: WS-E. Types: `quire_plan::srs`; cards:
`quire_markdown::srs`.

```json
{"v":1,"card":"k_3fa1c0d2b4e5","note":"Biology.md","at":1793000000000,"rating":3,"elapsed_days":1.0,"device":"d_…"}
{"v":1,"card":"k_9b20e41c7d3a","note":"Biology.md","at":1793000500000,"device":"d_…","from":"k_3fa1c0d2b4e5"}
```

* A review has `rating`: 1 Again, 2 Hard, 3 Good, 4 Easy. `elapsed_days`
  is what the device saw (whole days since the card's last review); replay
  doesn't need it.
* A line with `from` and no `rating` is a **rename**: the card `from`
  carries on as `card` (its question was edited, or ids were added).
* Card state is never stored: it is worked out by replaying every device's
  events in `(at, device)` order (`srs::merge_logs`, `srs::replay`), so all
  devices agree. Elapsed time is `floor(Δms / 86 400 000)`, which needs no
  time zone.
* Readers skip unparsable lines, lines with `v` above 1, and lines with
  neither `rating` nor `from`.

## Scheduling

FSRS-6 with its default parameters (the `fsrs` crate's) and a target
retention of 0.9. Again brings a card back in 10 minutes (in the same
session). Other answers schedule it after the FSRS interval in whole days,
at least one, with Anki's fuzz (±15 % to ±5 %, at least ±1 day from 2.5
days) seeded by the card id and its review count, so every device picks
the same day.

## Cards in notes

Cards are read from notes tagged `#flashcards` or with `srs: true`:

```markdown
What is ATP? :: The cell's energy currency
Name the powerhouse
of the cell
?
The mitochondrion
The ==nucleus== holds the ==DNA==.
```

* `Question :: Answer` on one line (not task lines; `::` inside `[…]` or
  `(…)` doesn't count, nor does `:::`).
* A paragraph, a line with only `?`, then the answer up to a blank line.
* Each `==highlight==` in a paragraph is a cloze card.
* The id is the card's `^block-id` (a paragraph of clozes adds `:1`, `:2`…;
  `^k-<12 hex>` means `k_<12 hex>`), else `k_` + the first 12 hex digits
  of the 64-bit FNV-1a hash of the folded question (lower case, single
  spaces, `*`, `_` and `` ` `` trimmed from words). A repeated question in
  a note gets `-2`, `-3`….
* "Add ids to this note's flashcards" writes `^k-…` ids (so later edits to
  the question keep the card) and logs renames where ids change.
