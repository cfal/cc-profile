use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use ccpd::{
    store::Store,
    types::{Error, Result},
};
use rusqlite::params;
use uuid::Uuid;

pub fn state(store: &Store) -> Result<(i64, Option<String>)> {
    Ok(store.db()?.query_row(
        "SELECT revision,verifier FROM panel_password WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}
pub fn revision(store: &Store) -> Result<i64> {
    Ok(store
        .db()?
        .query_row("SELECT revision FROM panel_password WHERE id=1", [], |r| {
            r.get(0)
        })?)
}
fn hashing_error() -> Error {
    Error::unavailable(
        "password_store",
        "Cannot use the stored panel password. Use ccpd set-password to reset it.",
    )
}
pub fn hash(password: &str) -> Result<String> {
    if password.len() > 1024 || password.chars().count() < 12 {
        return Err(Error::bad(
            "password_length",
            "Use at least 12 characters and at most 1024 bytes.",
        ));
    }
    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes()).map_err(|_| hashing_error())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| hashing_error())
}
pub fn matches(password: &str, verifier: &str) -> Result<bool> {
    if password.len() > 1024 {
        return Ok(false);
    }
    let parsed = PasswordHash::new(verifier).map_err(|_| hashing_error())?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(_) => Err(hashing_error()),
    }
}
pub fn already_set() -> Error {
    Error::conflict(
        "password_set",
        "A panel password is already set. Sign in instead.",
    )
}
pub fn save(store: &Store, hash: String, first_only: bool) -> Result<i64> {
    store.write(|db| {
        let changed = db.execute(
            "UPDATE panel_password SET verifier=?,revision=revision+1
            WHERE id=1 AND (NOT ? OR revision=0)",
            params![hash, first_only],
        )?;
        if changed != 1 {
            return Err(already_set());
        }
        Ok(
            db.query_row("SELECT revision FROM panel_password WHERE id=1", [], |r| {
                r.get(0)
            })?,
        )
    })
}
pub fn prompt() -> Result<()> {
    let state = ccpd::paths::state_dir()?;
    let database = Store::user_db_path(&state);
    eprintln!("Panel database: {}", database.display());
    let read = |prompt| {
        rpassword::prompt_password(prompt).map_err(|_| {
            Error::bad(
                "terminal",
                "Run ccpd set-password in an interactive terminal.",
            )
        })
    };
    let new = read("New panel password (at least 12 characters): ")?;
    if read("Confirm new password: ")? != new {
        return Err(Error::bad(
            "mismatch",
            "Passwords do not match. Nothing changed.",
        ));
    }
    let verifier = hash(&new)?;
    let store = Store::open_user(&state)?;
    save(&store, verifier, false)?;
    eprintln!(
        "Password set for {}. Previous browser sessions for this panel are signed out.",
        database.display()
    );
    Ok(())
}
