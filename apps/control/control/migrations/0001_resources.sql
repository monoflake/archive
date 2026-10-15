-- Every resource control holds, and every change made to them. A node and an account are their
-- ids, an app its id and its name. RFC o7eir.
CREATE TABLE resources (
	id TEXT PRIMARY KEY CHECK (length(id) = 16 AND id NOT GLOB '*[^0-9a-f]*'),
	kind TEXT NOT NULL CHECK (kind IN ('node', 'app', 'account')),
	name TEXT CHECK ((kind = 'app') = (name IS NOT NULL)),
	-- Canonical JSON, object keys sorted, so two specs compare as text.
	spec TEXT NOT NULL CHECK (json_valid(spec) AND json_type(spec) = 'object'),
	revision INTEGER NOT NULL CHECK (revision >= 0),
	updated_at TEXT NOT NULL,
	UNIQUE (kind, name)
) STRICT;

-- `kind` is `import` for the row an import writes at the revision it copied, so the next change
-- is numbered after it.
CREATE TABLE changes (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	at TEXT NOT NULL,
	by TEXT NOT NULL,
	kind TEXT NOT NULL CHECK (kind IN ('node', 'app', 'account', 'import')),
	resource TEXT,
	name TEXT,
	task TEXT UNIQUE,
	before TEXT CHECK (before IS NULL OR json_valid(before)),
	after TEXT CHECK (after IS NULL OR json_valid(after))
) STRICT;

CREATE INDEX changes_by_resource ON changes (resource, id);
