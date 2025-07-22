# keeper

## migrations

- scaffold migration (up only): `cargo sqlx migrate add -s {name}`
- scaffold migration (up/down): `cargo sqlx migrate add -s -r {name}`
- create database and run migrations `cargo sqlx database setup --database-url {connection-string}`
