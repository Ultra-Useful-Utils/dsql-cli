# Use tokio-postgres for shell semantics

The CLI uses `aws-sdk-dsql` for supported IAM token generation and `tokio-postgres` for the PostgreSQL connection instead of the official SQLx connector. A SQL shell needs streaming simple-query result framing and PostgreSQL cancellation tokens; those semantics outweigh the connector's simpler authentication and TLS assembly, which remain small explicit responsibilities in this project.
