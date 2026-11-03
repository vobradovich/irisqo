-- Since when an enqueued job waits to start: its scheduled time, or when it was (re)queued.
-- Feeds the irisqo.queue.lag metric.
-- now() is stable, so on Postgres 11+ this is a metadata-only change (no table rewrite);
-- the rows already queued get the time of the migration.
ALTER TABLE enqueued ADD COLUMN IF NOT EXISTS due_at timestamptz NOT NULL DEFAULT now();
