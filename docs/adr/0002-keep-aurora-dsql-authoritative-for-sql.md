# Keep Aurora DSQL authoritative for SQL support

The CLI sends lexically complete SQL to Aurora DSQL without a client-side allowlist. AWS documents a non-exhaustive and evolving PostgreSQL subset, while DSQL also adds syntax such as `AWS IAM GRANT`; a local grammar would become stale and reject valid work, so the CLI improves DSQL error diagnostics but never claims to enforce server compatibility.
