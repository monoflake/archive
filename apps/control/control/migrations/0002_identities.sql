-- Every identity an account holds, as two are compared: an email in lower case, any other exactly.
-- Its key is why no two accounts at one provider hold the same one, refused in the batch that
-- writes the account. Rebuilt whole by an import. RFC o7eir.
CREATE TABLE identities (
	provider TEXT NOT NULL,
	kind TEXT NOT NULL,
	value TEXT NOT NULL,
	account TEXT NOT NULL,
	PRIMARY KEY (provider, kind, value)
) STRICT;

CREATE INDEX identities_by_account ON identities (account);

-- What an import already wrote. SQLite's lower() folds ASCII alone; the next import writes every
-- row again as control compares it.
INSERT INTO identities (provider, kind, value, account)
SELECT
	json_extract(r.spec, '$.provider'),
	json_extract(i.value, '$.kind'),
	CASE json_extract(i.value, '$.kind')
		WHEN 'email' THEN lower(json_extract(i.value, '$.value'))
		ELSE json_extract(i.value, '$.value')
	END,
	r.id
FROM resources r, json_each(r.spec, '$.identities') i
WHERE r.kind = 'account';
