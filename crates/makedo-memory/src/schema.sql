PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY, agent TEXT NOT NULL, branch TEXT NOT NULL,
    created INTEGER NOT NULL, source TEXT UNIQUE
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY, session TEXT NOT NULL REFERENCES sessions(id),
    payload TEXT NOT NULL, created INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_session ON events(session, id);
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY, uid TEXT NOT NULL DEFAULT (lower(hex(randomblob(16)))),
    kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
    branch TEXT NOT NULL, session TEXT, source TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1,
    assessment TEXT NOT NULL DEFAULT 'hypothesis', supersedes INTEGER REFERENCES entries(id),
    created INTEGER NOT NULL, updated INTEGER NOT NULL,
    embedding BLOB, embedding_model TEXT,
    UNIQUE(kind, source)
);
CREATE INDEX IF NOT EXISTS entries_branch ON entries(branch, kind);
CREATE TABLE IF NOT EXISTS revisions (
    entry INTEGER NOT NULL REFERENCES entries(id), revision INTEGER NOT NULL,
    snapshot TEXT NOT NULL, PRIMARY KEY(entry, revision)
);
CREATE TABLE IF NOT EXISTS evidence (
    id INTEGER PRIMARY KEY, entry INTEGER NOT NULL REFERENCES entries(id), revision INTEGER NOT NULL,
    kind TEXT NOT NULL, target TEXT NOT NULL, sha TEXT NOT NULL, detail TEXT NOT NULL,
    created INTEGER NOT NULL, UNIQUE(entry, revision, kind, target, sha, detail)
);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    title, body, content='entries', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid,title,body) VALUES(new.id,new.title,new.body);
END;
CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts,rowid,title,body) VALUES('delete',old.id,old.title,old.body);
END;
CREATE TRIGGER IF NOT EXISTS entries_au AFTER UPDATE OF title,body ON entries BEGIN
    INSERT INTO entries_fts(entries_fts,rowid,title,body) VALUES('delete',old.id,old.title,old.body);
    INSERT INTO entries_fts(rowid,title,body) VALUES(new.id,new.title,new.body);
END;
