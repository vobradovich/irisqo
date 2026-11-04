//! Integration tests against a real Postgres.
//!
//! `#[sqlx::test]` creates a fresh database per test (and runs the migrations),
//! so the server from `DATABASE_URL` only needs the right to `CREATE DATABASE`.
//!
//! ```bash
//! DATABASE_URL=postgres://postgres@localhost/postgres cargo test --features integration-tests
//! ```

mod common;
mod delivery;
mod otel;
mod reliability;
mod security;
mod smoke;
mod telemetry;
mod ui;
mod ui_jobs;
