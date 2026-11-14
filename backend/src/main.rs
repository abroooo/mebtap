//! Binary entrypoint: wire env config -> pool -> migrations -> axum server.
//! All handler logic lives in the library (`backend::`) so it is testable.

use std::env;

use backend::{router, run_migrations, AppState};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ev:ev@localhost:5432/ev_tracker".to_string());
    let device_key = env::var("DEVICE_KEY").unwrap_or_else(|_| "dev".to_string());
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;
    run_migrations(&pool).await?;

    let app = router(AppState::new(pool, device_key));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("backend listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}
