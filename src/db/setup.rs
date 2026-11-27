//! The connection and the schema: what an instance (or `irisqo migrations run`) does before it works.

use std::{str::FromStr, time::Duration};

use sqlx::{
    Pool, Postgres,
    migrate::Migrator,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::models::DbOptions;

/// The migrations of `sql/migrations`, embedded in the binary; a const, so that each run can
/// add its schema.
const MIGRATOR: Migrator = sqlx::migrate!("./sql/migrations");

/// A pool whose connections see the tables of the schema first.
pub async fn connect(db: &DbOptions, app_name: &str) -> Pool<Postgres> {
    let conn = PgConnectOptions::from_str(&db.url)
        .expect("Unable to parse the database URL")
        .application_name(app_name)
        // every statement and every function sees the tables of the schema
        .options([("search_path", db.schema.as_str())])
        // PgBouncer refuses it, and since Postgres 12 the default (1) is exact already
        .extra_float_digits(None);
    PgPoolOptions::new()
        .max_connections(db.connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(conn)
        .await
        .expect("Unable to connect to Postgres")
}

/// Drops the schema with everything in it. The caller checks the environment: this is for
/// development and tests (`irisqo migrations run --recreate`).
pub async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"
    )))
    .execute(pool)
    .await
    .expect("Unable to drop the schema");
}

/// Creates the schema when missing (unless `db.create_schema` is false), then runs the migrations
/// in it, under the session lock of sqlx: a transaction pooler breaks it (see `docs/migrations.md`).
pub async fn migrate_schema(pool: &Pool<Postgres>, db: &DbOptions) {
    let mut migrator = MIGRATOR;
    if db.create_schema {
        // valid (`Options::validate`), quoted all the same: sqlx inserts it as it is
        migrator.create_schema(format!("\"{}\"", db.schema));
    }
    migrator.run(pool).await.expect("Unable to run migrations");
}

/// The SQL of the migrations, in order, as one script for a tool other than `irisqo migrations run`
/// (Flyway, Liquibase, EF, psql). It starts by creating the schema (unless `create_schema` is
/// false) and making it the default one, as the connections of the server do: the migrations
/// name no schema.
///
/// `schema` is quoted into the script, so it must be valid (`Options::validate` checks it).
pub fn migrations_sql(schema: &str, create_schema: bool) -> String {
    let mut sql = if create_schema {
        format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\";\n")
    } else {
        String::new()
    };
    sql += &format!("SET search_path TO \"{schema}\";\n");
    for migration in MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
    {
        let body = migration.sql.as_str();
        sql += &format!(
            "\n-- migration {}: {}\n{body}",
            migration.version, migration.description
        );
        if !body.ends_with('\n') {
            sql.push('\n');
        }
    }
    sql
}

#[test]
fn the_script_has_every_migration_after_the_schema() {
    let sql = migrations_sql("mine", true);
    assert!(
        sql.starts_with("CREATE SCHEMA IF NOT EXISTS \"mine\";\nSET search_path TO \"mine\";\n")
    );
    assert!(sql.contains("\n-- migration 1: init\n"));
    assert!(sql.contains("CREATE TABLE instances ("));
    assert!(sql.ends_with('\n'));
    let existing = migrations_sql("mine", false);
    assert!(existing.starts_with("SET search_path TO \"mine\";\n"));
    assert!(existing.contains("CREATE TABLE instances ("));
}
