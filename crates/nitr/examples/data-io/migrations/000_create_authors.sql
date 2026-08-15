CREATE TABLE authors (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
INSERT INTO authors (id, name) VALUES (1, 'ada'), (2, 'grace');
