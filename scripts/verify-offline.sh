#!/usr/bin/env bash
# Linux network-namespace verification. Run from the Lore project root.
set -euo pipefail
lore_binary="${LORE_BIN:-$PWD/target/release/lore}"
lore_corpus="${1:-../undercurrent}"
lore_index="${2:-.lore}"
lore_index_args=()
lore_query_args=()
if [[ -n "${LORE_PASSAGE_SCORING:-}" ]]; then
  lore_index_args=(--content)
  lore_query_args=(--passage-scoring "$LORE_PASSAGE_SCORING")
fi
unshare --user --map-root-user --net -- "$lore_binary" --index-dir "$lore_index" index "$lore_corpus" --json "${lore_index_args[@]}"
unshare --user --map-root-user --net -- "$lore_binary" --index-dir "$lore_index" brief \
  "How do I read output without skipping bytes?" --json "${lore_query_args[@]}"
unshare --user --map-root-user --net -- "$lore_binary" --index-dir "$lore_index" related \
  docs/sdk/python/streams.md --limit 3 --json
unshare --user --map-root-user --net -- "$lore_binary" --index-dir "$lore_index" diagnose \
  "How do I read output without skipping bytes?" --scope docs --no-connections "${lore_query_args[@]}"
unshare --user --map-root-user --net -- "$lore_binary" --index-dir "$lore_index" batch --scope docs --graph "${lore_query_args[@]}" <<'JSONL'
"How do I read output without skipping bytes?"
"What happens when the launch reply is lost?"
JSONL
