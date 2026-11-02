# Flashcard reviews: `.quire/shared/srs/<device-id>.jsonl`

One file per device, so devices never write the same file; it travels with
the vault. Owner: WS-E. Types: `quire_plan::srs`.

```json
{"v":1,"card":"k_3fa1c0d2b4e5","note":"Biology.md","at":1793000000000,"rating":3,"elapsed_days":1.5,"device":"d_…"}
```

* `card`: the card's `^id`, or `k_` + 12 hex of the folded question text.
* `rating`: 1 Again, 2 Hard, 3 Good, 4 Easy.
* Card state is never stored: it is worked out by replaying every device's
  events in `(at, device)` order (`srs::merge_logs`), so all devices agree.
* Readers skip unparsable lines and lines with `v` above 1.
