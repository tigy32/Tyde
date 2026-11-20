# Response metrics

The server records how fast every backend answers so trends can be compared
across backends, models and session settings. There is no UI; query the data
with `sqlite3`.

## Where

`metrics.sqlite` beside the session store (`~/.tyde/metrics.sqlite` by
default). Rows are kept forever.

## What is recorded

- `response_samples`: one row per model response. Time to first token from
  the input that triggered it (`ttft_from_input_ms`) and from stream start,
  generation time, token counts, and output tokens per second (streamed
  responses with known usage only). Late usage reported by a metadata update
  is attached before the row is written.
- `turn_samples`: one row per backend turn: wall time, time inside model
  responses versus everything else (tools, waits), summed tokens, outcome.
- `daily_rollups`: per UTC day, backend, model and settings key: counts,
  sums and fixed histogram buckets for TTFT and tokens per second.
- `writer_stats`: samples lost because the queue was full or a write failed.

`settings_key` is the agent's session settings as sorted `name=value` pairs,
so a backend + settings combination groups with `GROUP BY backend,
settings_key`. Backend-native sub-agents have no settings key.

## Contention

Agents never touch SQLite. Each agent actor folds its own live events; stream
deltas only stamp an instant. Finished samples go to one writer thread over a
bounded channel with a non-blocking send, so a full queue drops and counts a
sample instead of stalling an agent. The writer commits everything queued in
one WAL transaction, so batches grow with load.

## Example queries

```sql
-- Daily mean TTFT and tokens/s per backend and model.
SELECT day, backend, model,
       ttft_from_input_ms_sum * 1.0 / ttft_samples AS mean_ttft_ms,
       tps_output_tokens_sum * 1000.0 / tps_generation_ms_sum AS tokens_per_s
FROM daily_rollups WHERE ttft_samples > 0 AND tps_generation_ms_sum > 0
ORDER BY day DESC;

-- Compare settings for one backend over the last week.
SELECT settings_key, COUNT(*), AVG(ttft_from_input_ms), AVG(output_tokens_per_second)
FROM response_samples
WHERE backend = 'claude' AND ended_at_ms > (strftime('%s','now') - 7*86400) * 1000
GROUP BY settings_key;
```
