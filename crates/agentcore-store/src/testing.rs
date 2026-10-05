//! Helpers for tests that need a real PostgreSQL.

use crate::{Cipher, Store};

/// Env var with a superuser connection URL (e.g. `postgres://postgres@localhost/postgres`).
pub const TEST_DATABASE_ENV: &str = "AGENTCORE_TEST_DATABASE_URL";

/// Create a fresh, uniquely named database and connect a [`Store`] to it.
/// Returns `None` (and prints why) when `AGENTCORE_TEST_DATABASE_URL` is unset.
pub async fn fresh_store() -> Option<(Store, String)> {
    let Ok(admin_url) = std::env::var(TEST_DATABASE_ENV) else {
        eprintln!("skipping: set {TEST_DATABASE_ENV} to run PostgreSQL tests");
        return None;
    };
    let name = format!("agentcore_test_{}", uuid::Uuid::new_v4().simple());
    let admin = sqlx::PgPool::connect(&admin_url)
        .await
        .expect("connect admin db");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test db");
    admin.close().await;
    let base = admin_url
        .rsplit_once('/')
        .map(|(b, _)| b)
        .unwrap_or(&admin_url);
    let url = format!("{base}/{name}");
    let store = Store::connect(&url, Cipher::from_key(&[42; 32]))
        .await
        .expect("connect test db");
    Some((store, url))
}
