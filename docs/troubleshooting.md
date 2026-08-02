# Troubleshooting

## Region or profile cannot be resolved

Pass `--region`, use a cluster ARN or canonical endpoint that encodes a Region,
or configure the AWS SDK Region provider chain. A direct cluster ID has no
Region. Use `--profile` only for an SDK profile that can supply cached or other
supported credentials; `dsql` does not log in to IAM Identity Center for you.
An explicit `--region` that conflicts with an ARN or endpoint is rejected.

## Discovery is empty or access is denied

A discoverable cluster is merely one returned by `ListClusters` for the active
identity; it is not proof of connection authorization. Check the discovery
policy in [IAM permissions](iam.md), Region, and profile. In an interactive
shell, enter a cluster ID, ARN, or canonical endpoint manually when inventory
is unavailable. Direct selection does not require discovery permission.

## Database role or connection access is denied

`admin` is an elevated database role and needs `dsql:DbConnectAdmin`. A custom
database role needs `dsql:DbConnect` and an IAM-to-database-role association in
the cluster. Confirm the exact PostgreSQL role name supplied with `-U`; token
generation alone does not establish that the role exists or is mapped.

## TLS connection fails

Use a canonical Aurora DSQL endpoint and do not attempt to disable TLS
verification. Check system trust roots, endpoint hostname, clock, and an
additive PEM file supplied with `--ssl-root-cert`. A malformed PEM, unknown CA,
or hostname mismatch is intentionally rejected.

## SQL reports an OCC conflict or uncertain outcome

Aurora DSQL is authoritative for SQL support. `dsql` does not use a client-side
SQL allowlist and does not automatically retry SQL. For an `OC000` or `OC001`
conflict, review the transaction and explicitly resubmit it if safe. After a
disconnect following submission, the transaction outcome may be unknown; inspect
server state before submitting anything again.

## Connection refresh, catalog completion, or metrics

Aurora DSQL limits connection lifetime. The shell proactively performs a
**session refresh** only between statements, normally before 55 minutes, and
never restores transaction state or replays SQL. Use `\refresh` after DDL when
no transaction is active to rebuild cached completion metadata.

`\metrics` needs `cloudwatch:GetMetricData` on `*`. It shows missing samples as
`No data`, not zero, and returns safely to the interactive shell when denied.
See [IAM permissions](iam.md).

## Terminal recovery

If an interactive terminal appears stuck, press Ctrl+C once to stop further
result output and cancel the query, or twice to leave the shell; Ctrl+D exits
only from an empty buffer. If a pager
misbehaves, leave it and run `\pager off` after returning. Terminal state is
restored on normal exits, errors, signals, and panic paths; if a host-level
termination prevents restoration, start a new terminal session.
