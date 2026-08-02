# Aurora DSQL CLI

This context defines the language used by the `dsql` command when selecting and connecting to Amazon Aurora DSQL clusters.

## Language

**Cluster**:
A regional Aurora DSQL resource with one managed PostgreSQL endpoint and one built-in database named `postgres`.
_Avoid_: Database, instance

**Discoverable cluster**:
A cluster returned by `ListClusters` for the active AWS identity in the selected Region. Discoverability does not imply permission to connect.
_Avoid_: Accessible cluster, authorized cluster

**Cluster selector**:
A cluster ID, cluster ARN, or canonical Aurora DSQL endpoint supplied to identify a connection target.
_Avoid_: Cluster name, alias

**Database role**:
The PostgreSQL role sent as the connection username and used for database authorization.
_Avoid_: IAM role, AWS role

**Admin role**:
The predefined Aurora DSQL database role named `admin`, whose connections require `dsql:DbConnectAdmin`.
_Avoid_: Root user, superuser

**Custom role**:
A named PostgreSQL database role created in a cluster and associated with an IAM principal, whose connections require `dsql:DbConnect`.
_Avoid_: User mode, regular user

**Interactive shell**:
The terminal session that accepts SQL and supported backslash commands after establishing one cluster connection.
_Avoid_: psql, console

**Metrics dashboard**:
The temporary full-screen terminal view of CloudWatch metrics for the cluster connected by the interactive shell.
_Avoid_: CloudWatch dashboard, monitoring console

**Session refresh**:
Replacement of an idle database connection before or after Aurora DSQL's connection limit, without replaying SQL or restoring transaction state.
_Avoid_: Retry, failover
