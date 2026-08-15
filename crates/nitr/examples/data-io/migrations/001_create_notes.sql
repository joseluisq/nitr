-- Migrations are plain SQL, applied in numeric order, each in its own
-- transaction, and recorded in `_nitr_migrations` so they never run twice.
CREATE TABLE notes (
    id         INTEGER PRIMARY KEY,
    author_id  INTEGER NOT NULL REFERENCES authors(id),
    body       TEXT    NOT NULL,
    created_at TEXT    NOT NULL DEFAULT (datetime('now'))
);
