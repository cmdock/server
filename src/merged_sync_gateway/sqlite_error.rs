//! SQLite error classifiers shared inside the merged sync gateway.

pub(super) fn is_sqlite_constraint_violation(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::ConstraintViolation,
                    ..
                },
                _
            ))
        )
    })
}
