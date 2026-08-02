# IAM Permissions

Grant only the statements needed for the commands and database role a principal uses. The policies below are independent and can be combined. Replace the uppercase placeholders with the target AWS account, Region, and cluster ID.

`sts:GetCallerIdentity` is optional. `dsql` uses it only to show best-effort identity context; a denial warns without blocking discovery or connection.

## Cluster Discovery

Discovery lists clusters and retrieves each cluster's details. `ListClusters` cannot be scoped to a cluster. The wildcard cluster ARN lets discovery enrich every cluster returned in the selected Region.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "dsql:ListClusters",
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": "dsql:GetCluster",
      "Resource": "arn:aws:dsql:REGION:ACCOUNT_ID:cluster/*"
    }
  ]
}
```

Supplying a cluster selector directly does not require discovery permissions.

## Admin Role Connection

Connections using the predefined `admin` database role require `dsql:DbConnectAdmin`:

```json
{
  "Version": "2012-10-17",
  "Statement": {
    "Effect": "Allow",
    "Action": "dsql:DbConnectAdmin",
    "Resource": "arn:aws:dsql:REGION:ACCOUNT_ID:cluster/CLUSTER_ID"
  }
}
```

## Custom Role Connection

Connections using a custom database role require `dsql:DbConnect` and an IAM-to-database-role association configured in the cluster:

```json
{
  "Version": "2012-10-17",
  "Statement": {
    "Effect": "Allow",
    "Action": "dsql:DbConnect",
    "Resource": "arn:aws:dsql:REGION:ACCOUNT_ID:cluster/CLUSTER_ID"
  }
}
```

`dsql:DbConnect` does not grant access to the `admin` database role, and `dsql:DbConnectAdmin` does not grant access to custom roles.

## Metrics Dashboard

The metrics dashboard batches its queries through CloudWatch `GetMetricData`. It does not call `ListMetrics`, create dashboards, or create alarms. CloudWatch metric reads do not support cluster-level resource scoping, so the action requires `Resource: "*"`:

```json
{
  "Version": "2012-10-17",
  "Statement": {
    "Effect": "Allow",
    "Action": "cloudwatch:GetMetricData",
    "Resource": "*"
  }
}
```

The dashboard displays an actionable permission error and returns safely to the interactive shell when this action is denied.
