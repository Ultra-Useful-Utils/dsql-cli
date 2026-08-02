# Provide a DSQL-native shell instead of cloning psql

The interactive client adopts familiar SQL editing, output, and a curated set of backslash commands without promising general `psql` compatibility. DSQL has a single database, IAM authentication, fixed connection lifetime, different catalog behavior, and CloudWatch operations, so a native contract avoids inheriting unsupported scripting and administration behavior while allowing purpose-built features such as `\refresh` and `\metrics`.
