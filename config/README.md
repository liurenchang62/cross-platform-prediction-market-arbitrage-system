# Matching Configuration

`categories.toml` defines the broad market taxonomy used to partition contract retrieval and apply category-aware matching behavior.

## Role in the pipeline

The taxonomy serves three purposes:

1. reduce the number of unrelated title comparisons;
2. build vocabulary and TF-IDF statistics within a more coherent market group;
3. provide context for deterministic validators that distinguish, for example, a match winner from a score, handicap, player-stat, or tournament market.

Category membership is only a retrieval constraint. It does not establish economic equivalence or authorize execution.

## Editing guidance

- Use lower-case, event-specific keywords rather than broad words that occur across most markets.
- Check whether a keyword can place one listing in multiple categories; ambiguous assignments increase comparisons and false candidates.
- Preserve TOML syntax and unique category names.
- Run the trader test suite after changing keywords because category membership changes candidate generation.
- Review retained pairs manually when adding a new event family or settlement convention.

```bash
cargo test -p trader
```

Thresholds, sizing limits, refresh intervals, and the live-order flag are not stored here. They are source-level controls in `trader/src/system_params.rs` and are documented in [`../docs/CONFIGURATION.md`](../docs/CONFIGURATION.md).
