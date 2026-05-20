-- Baseline state for the broken-prompt demo.
-- Idempotent: safe to re-run; truncates first.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS episodes (
    id    bigserial PRIMARY KEY,
    text  text      NOT NULL
);

TRUNCATE episodes RESTART IDENTITY;

INSERT INTO episodes (text) VALUES
    ('Customer asked about pricing — sent FAQ link.'),
    ('Customer reported a login issue; escalated to engineering.'),
    ('Customer wanted feature X — added to product backlog.'),
    ('Customer cancelled trial; sent feedback survey.'),
    ('Customer thanked support for the quick reply.');
