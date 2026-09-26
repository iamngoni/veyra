//! Build script: rebuild when a migration is added or changed.
//!
//! `sqlx::migrate!` embeds `migrations/` at compile time, but Cargo only
//! tracks Rust sources. Without this, an incremental build (including the
//! cached Docker build) could ship a binary missing a new migration.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
