//! Task-key prefix derivation + DB-side application per
//! `task-write-contract.md` § Task Keys (cmdock/architecture commit
//! 1a7af9e — `cmdock/architecture#34`).
//!
//! `derive_prefix` is pure. `apply_prefix` and `backfill_missing_user_prefixes`
//! call into `ConfigStore` and emit the operator-facing audit + metric pair.
//! v1 maps account = user; the derive algorithm is byte-deterministic so
//! cmdock-admin / iOS / obsidian implementations can reproduce the same
//! result client-side without round-tripping to the server.

use std::collections::HashSet;

use uuid::Uuid;

use crate::store::{ConfigStore, StoreError};

pub const SOURCE_SIGNUP: &str = "signup";
pub const SOURCE_BOOTSTRAP: &str = "bootstrap";
pub const SOURCE_BACKFILL: &str = "backfill";
pub const SOURCE_OPERATOR: &str = "operator";

/// Derive a `^[A-Z][A-Z0-9]{0,9}$` prefix for a user.
///
/// 1. ASCII-alphanumeric filter on `username` + uppercase.
/// 2. If empty or first char is digit, prepend `'U'`.
/// 3. Truncate to 10 chars.
/// 4. If colliding, append a 2-digit numeric suffix (`02`..`99`) and shrink
///    the base to keep ≤10 char total. First non-collision wins.
/// 5. Last-resort: `U<hex>` from `user_id` simple form, growing the hex
///    tail from 4 to 9 chars until non-collision.
/// 6. Final fallback (operationally unreachable absent malicious test
///    setup): the 9-hex form regardless of collision. Asserted unreachable
///    in tests.
pub fn derive_prefix(
    username: &str,
    user_id: &Uuid,
    existing_prefixes: &HashSet<String>,
) -> String {
    // Step 1: ASCII alphanumeric filter + uppercase.
    let mut s: String = username
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();

    // Step 2: ensure first char is [A-Z].
    if s.is_empty() || s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.insert(0, 'U');
    }

    // Step 3: truncate to 10 ASCII chars (filter step guarantees 1-byte chars).
    s.truncate(10);

    // Step 4: first-occurrence — return immediately on no-collision.
    if !existing_prefixes.contains(&s) {
        return s;
    }

    // Step 4 cont.: 2-digit suffix loop. Suffix is 2 bytes, so base is
    // truncated to 8 ASCII chars to keep the total ≤10.
    for n in 2..=99u8 {
        let suffix = format!("{n:02}");
        let base_len = 10 - suffix.len();
        let mut candidate: String = s.chars().take(base_len).collect();
        candidate.push_str(&suffix);
        if !existing_prefixes.contains(&candidate) {
            return candidate;
        }
    }

    // Step 5: hex fallback. simple form is 32 lowercase hex chars (no hyphens).
    let hex = user_id.simple().to_string();
    let hex = hex.to_ascii_uppercase();
    for hex_len in 4..=9 {
        let candidate = format!("U{}", &hex[..hex_len]);
        if !existing_prefixes.contains(&candidate) {
            return candidate;
        }
    }

    // Step 6: full 9-hex tail regardless of collision. Reaching this branch
    // requires 99 step-4 collisions AND 6 step-5 collisions — operationally
    // impossible without crafted test fixtures. Returning here keeps the
    // function total; uniqueness is then enforced at the DB UNIQUE index
    // (which would surface a `users.prefix` constraint violation, prompting
    // operator override).
    format!("U{}", &hex[..9])
}

/// Validate a prefix string against the canonical format
/// `^[A-Z][A-Z0-9]{0,9}$`. Returns the validated prefix on success;
/// human-readable error on rejection.
pub fn validate_prefix_format(prefix: &str) -> Result<&str, String> {
    if prefix.is_empty() {
        return Err("prefix is empty".into());
    }
    if prefix.len() > 10 {
        return Err(format!(
            "prefix must be ≤10 characters (got {})",
            prefix.len()
        ));
    }
    let mut chars = prefix.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return Err(format!("prefix must start with [A-Z] (got '{first}')"));
    }
    for c in chars {
        if !(c.is_ascii_uppercase() || c.is_ascii_digit()) {
            return Err(format!("prefix must match [A-Z0-9]+ (got '{c}')"));
        }
    }
    Ok(prefix)
}

/// Apply a prefix to a user via `set_user_prefix`, then materialise the
/// Runtime User's Personal Task Scope.
///
/// `account.prefix_set` audit + `task_keys_prefix_assigned_total` are emitted
/// immediately after the prefix write commits. The follow-up Task Scope ensure
/// emits `account.task_scope_ensured` plus `task_scope_ensure_total`; failures
/// emit `task_scope_ensure_total{result="error"}`. If scope ensure fails, the
/// prefix is still durably committed and audited; the function returns an error
/// so the operator sees the partial failure. `backfill_personal_task_scopes` is
/// the canonical repair path for that prefix-set / scope-missing state.
///
/// Prefixes are mutable only before the active Personal Task Scope exists;
/// after scope materialisation, `set_user_prefix` rejects changes with
/// `PREFIX_LOCKED` so `users.prefix == task_scopes.key_prefix` cannot drift.
///
/// # Sources
///
/// `source` should be one of `SOURCE_SIGNUP`, `SOURCE_BOOTSTRAP`,
/// `SOURCE_BACKFILL`, or `SOURCE_OPERATOR`.
pub async fn apply_prefix(
    store: &dyn ConfigStore,
    user_id: &str,
    prefix: &str,
    source: &'static str,
) -> anyhow::Result<()> {
    match store.set_user_prefix(user_id, prefix).await {
        Ok(()) => {
            metrics::counter!(
                "task_keys_prefix_assigned_total",
                "source" => source,
            )
            .increment(1);
            tracing::info!(
                target: "audit",
                action = "account.prefix_set",
                source,
                user_id,
                prefix,
            );

            let ensured = ensure_personal_task_scope_with_telemetry(store, user_id, source).await?;
            tracing::info!(
                target: "audit",
                action = "account.task_scope_ensured",
                source,
                user_id,
                task_scope_id = %ensured.scope.id,
                prefix = %ensured.scope.key_prefix,
                result = ensured.result,
            );
            Ok(())
        }
        Err(StoreError::PrefixLocked) => Err(anyhow::anyhow!(
            "PREFIX_LOCKED: cannot change prefix after task allocations exist \
             or the user's first-access task-keys backfill has run"
        )),
        Err(err) if err.is_unique(crate::store::error::resources::USERS_PREFIX) => Err(
            anyhow::anyhow!("PREFIX_TAKEN: prefix '{prefix}' is already used by another user"),
        ),
        Err(err) => Err(anyhow::anyhow!("set_user_prefix failed: {err}")),
    }
}

struct ScopeEnsureTelemetry {
    scope: crate::store::models::TaskScopeRecord,
    result: &'static str,
}

enum ScopeEnsureOutcome {
    Created,
    Existing,
}

impl ScopeEnsureOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Existing => "existing",
        }
    }
}

async fn ensure_personal_task_scope_with_telemetry(
    store: &dyn ConfigStore,
    user_id: &str,
    source: &'static str,
) -> anyhow::Result<ScopeEnsureTelemetry> {
    let ensured = match store.ensure_personal_task_scope_for_user(user_id).await {
        Ok(ensured) => ensured,
        Err(err) => {
            metrics::counter!("task_scope_ensure_total", "source" => source, "result" => "error")
                .increment(1);
            return Err(scope_ensure_error(err));
        }
    };
    let outcome = if ensured.created {
        ScopeEnsureOutcome::Created
    } else {
        ScopeEnsureOutcome::Existing
    };
    let result = outcome.label();
    metrics::counter!("task_scope_ensure_total", "source" => source, "result" => result)
        .increment(1);
    Ok(ScopeEnsureTelemetry {
        scope: ensured.scope,
        result,
    })
}

fn scope_ensure_error(err: StoreError) -> anyhow::Error {
    if err.is_unique(crate::store::error::resources::TASK_SCOPES_KEY_PREFIX) {
        anyhow::anyhow!(
            "TASK_SCOPE_PREFIX_INCONSISTENT: users.prefix is assigned but an existing Personal \
             Task Scope already claims that prefix; operator inspection required"
        )
    } else if err.is_unique(crate::store::error::resources::TASK_SCOPES_PERSONAL_OWNER) {
        anyhow::anyhow!(
            "TASK_SCOPE_ALREADY_EXISTS: user already has a Personal Task Scope \
             (unexpected: ensure normally re-reads and returns the existing row)"
        )
    } else {
        anyhow::anyhow!("ensure_personal_task_scope failed: {err}")
    }
}

/// Walk users with NULL prefix and assign one via `derive_prefix`.
/// Idempotent — second run finds no NULL rows. Called from server
/// startup (post-migration hook in `main.rs`) and exposed for direct
/// invocation by integration tests.
pub async fn backfill_missing_user_prefixes(store: &dyn ConfigStore) -> anyhow::Result<usize> {
    let pending = store.users_without_prefix().await?;
    if pending.is_empty() {
        return Ok(0);
    }

    let users = store.list_users().await?;
    let mut taken: HashSet<String> = HashSet::new();
    for u in &users {
        if let Some(p) = store.get_user_prefix(&u.id).await? {
            taken.insert(p);
        }
    }

    let mut count = 0;
    for u in pending {
        let user_uuid = Uuid::parse_str(&u.id).unwrap_or_else(|_| Uuid::nil());
        let derived = derive_prefix(&u.username, &user_uuid, &taken);
        taken.insert(derived.clone());
        match apply_prefix(store, &u.id, &derived, SOURCE_BACKFILL).await {
            Ok(()) => count += 1,
            Err(err) => {
                metrics::counter!("task_keys_prefix_backfill_errors_total").increment(1);
                tracing::warn!(
                    user_id = %u.id,
                    username = %u.username,
                    prefix = %derived,
                    error = %err,
                    "Prefix backfill failed for user; continuing"
                );
            }
        }
    }
    Ok(count)
}

/// Ensure every prefixed Runtime User has an explicit Personal Task Scope.
/// Must run after `backfill_missing_user_prefixes`; lazy callers must follow
/// the same order. Idempotent — second run returns 0. This is also the
/// canonical repair path for a partial state where a prefix was committed but
/// Personal Task Scope materialisation failed before returning to the caller.
pub async fn backfill_personal_task_scopes(store: &dyn ConfigStore) -> anyhow::Result<usize> {
    let users = store.list_users_pending_personal_task_scope().await?;
    let mut count = 0;
    for user in users {
        match ensure_personal_task_scope_with_telemetry(store, &user.id, "backfill").await {
            Ok(ensured) => {
                tracing::info!(
                    target: "audit",
                    action = "account.task_scope_ensured",
                    source = "backfill",
                    user_id = %user.id,
                    username = %user.username,
                    task_scope_id = %ensured.scope.id,
                    prefix = %user.prefix,
                    result = ensured.result,
                );
                if ensured.result == "created" {
                    count += 1;
                }
            }
            Err(err) => {
                metrics::counter!("task_scope_backfill_errors_total").increment(1);
                tracing::warn!(
                    user_id = %user.id,
                    username = %user.username,
                    prefix = %user.prefix,
                    error = %err,
                    "Personal Task Scope backfill failed for user; continuing"
                );
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::store::models::NewUser;
    use crate::store::sqlite::SqliteConfigStore;

    struct TestStore {
        store: Arc<SqliteConfigStore>,
        _tmp: tempfile::TempDir,
    }

    async fn migrated_store() -> TestStore {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("config.sqlite");
        let store = Arc::new(SqliteConfigStore::new(&db.to_string_lossy()).await.unwrap());
        store.run_migrations().await.unwrap();
        TestStore { store, _tmp: tmp }
    }

    async fn create_user(store: &SqliteConfigStore, username: &str) -> String {
        store
            .create_user(&NewUser {
                username: username.to_string(),
                password_hash: "hash".to_string(),
            })
            .await
            .unwrap()
            .id
    }

    fn no_existing() -> HashSet<String> {
        HashSet::new()
    }

    fn empty_user_id() -> Uuid {
        Uuid::nil()
    }

    #[test]
    fn ascii_uppercase_simple() {
        let p = derive_prefix("alice", &empty_user_id(), &no_existing());
        assert_eq!(p, "ALICE");
    }

    #[test]
    fn drops_punctuation_and_unicode() {
        let p = derive_prefix("a-li.ce_w📚", &empty_user_id(), &no_existing());
        assert_eq!(p, "ALICEW");
    }

    #[test]
    fn empty_username_gets_u_prefix() {
        // After filter + length-0 → step 2 prepends 'U'.
        let p = derive_prefix("", &empty_user_id(), &no_existing());
        assert_eq!(p, "U");
    }

    #[test]
    fn all_unicode_username_gets_u_prefix() {
        let p = derive_prefix("🦀🦀🦀", &empty_user_id(), &no_existing());
        assert_eq!(p, "U");
    }

    #[test]
    fn digit_first_username_gets_u_prepended() {
        let p = derive_prefix("123abc", &empty_user_id(), &no_existing());
        assert_eq!(p, "U123ABC");
    }

    #[test]
    fn truncates_long_username_to_10() {
        let p = derive_prefix("alicewasinwonderland", &empty_user_id(), &no_existing());
        assert_eq!(p.len(), 10);
        assert_eq!(p, "ALICEWASIN");
    }

    #[test]
    fn collision_appends_numeric_suffix_keeping_10_max() {
        let mut taken = HashSet::new();
        taken.insert("ALICE".to_string());
        let p = derive_prefix("alice", &empty_user_id(), &taken);
        assert_eq!(p, "ALICE02");
    }

    #[test]
    fn ten_char_collision_shrinks_base_for_suffix() {
        let mut taken = HashSet::new();
        taken.insert("ALICEWASIN".to_string());
        let p = derive_prefix("alicewasinwonderland", &empty_user_id(), &taken);
        // Base shrinks to 8 chars, suffix "02" → 10 total.
        assert_eq!(p, "ALICEWAS02");
        assert_eq!(p.len(), 10);
    }

    #[test]
    fn ninety_nine_collisions_fall_through_to_hex() {
        let mut taken = HashSet::new();
        taken.insert("ALICE".to_string());
        // Suffix shrink rule: base = take(10 - 2) = take(8); for "ALICE"
        // (5 chars) take(8) is the whole word, so candidates are
        // "ALICE02".."ALICE99". Populate them all to force step 5.
        for n in 2..=99u8 {
            taken.insert(format!("ALICE{n:02}"));
        }
        let user_id = Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap();
        let p = derive_prefix("alice", &user_id, &taken);
        assert!(p.starts_with('U'), "expected hex fallback, got {p}");
        assert_eq!(p, "U0123");
    }

    #[test]
    fn hex_fallback_grows_on_collision() {
        let mut taken = HashSet::new();
        taken.insert("U".to_string());
        // 99 step-4 collisions (U02..U99 collapse to U02 since suffix
        // expands base "U" → "U" + "02"). Force fallthrough by also
        // taking U with all 2-suffixed forms.
        for n in 2..=99u8 {
            taken.insert(format!("U{n:02}"));
        }
        // Plus the first hex candidate "U0123" so we have to grow.
        taken.insert("U0123".to_string());
        let user_id = Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap();
        let p = derive_prefix("", &user_id, &taken);
        assert_eq!(p, "U01234");
    }

    #[test]
    fn hex_fallback_uppercases() {
        let user_id = Uuid::parse_str("abcdefab-cdef-abcd-efab-cdefabcdefab").unwrap();
        let mut taken = HashSet::new();
        taken.insert("ALICE".to_string());
        for n in 2..=99u8 {
            taken.insert(format!("ALICE{n:02}"));
        }
        let p = derive_prefix("alice", &user_id, &taken);
        assert!(p.starts_with('U'));
        // Hex should be uppercase, matching the canonical prefix charset.
        assert_eq!(p, "UABCD");
    }

    #[test]
    fn validate_prefix_format_accepts_canonical() {
        for ok in ["A", "AB", "WORK", "ABC123", "X1234567Y9"] {
            assert!(validate_prefix_format(ok).is_ok(), "expected {ok} to pass");
        }
    }

    #[test]
    fn validate_prefix_format_rejects_non_canonical() {
        for bad in ["", "1ABC", "abc", "WORK!", "ACME-EU", "ELEVENCHARS"] {
            assert!(
                validate_prefix_format(bad).is_err(),
                "expected {bad} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn backfill_personal_task_scopes_skips_user_without_prefix() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user_id = create_user(&store, "no-prefix").await;

        let count = backfill_personal_task_scopes(store.as_ref()).await.unwrap();

        assert_eq!(count, 0);
        assert!(
            store
                .get_personal_task_scope_for_user(&user_id)
                .await
                .unwrap()
                .is_none(),
            "no-prefix user must not get a placeholder Personal Task Scope"
        );
    }

    #[tokio::test]
    async fn apply_prefix_locks_after_personal_task_scope_exists() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user_id = create_user(&store, "prefix-lock").await;

        apply_prefix(store.as_ref(), &user_id, "FIRST", SOURCE_OPERATOR)
            .await
            .unwrap();
        let err = apply_prefix(store.as_ref(), &user_id, "SECOND", SOURCE_OPERATOR)
            .await
            .expect_err("active Personal Task Scope should lock prefix changes");

        assert!(err.to_string().contains("PREFIX_LOCKED"));
        assert_eq!(
            store.get_user_prefix(&user_id).await.unwrap().as_deref(),
            Some("FIRST")
        );
        let scope = store
            .get_personal_task_scope_for_user(&user_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope.key_prefix, "FIRST");
    }

    #[tokio::test]
    async fn backfill_personal_task_scopes_skips_existing_scope_and_second_run_is_zero() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user_id = create_user(&store, "existing-scope").await;
        store.set_user_prefix(&user_id, "EXIST").await.unwrap();
        let existing = store
            .ensure_personal_task_scope_for_user(&user_id)
            .await
            .unwrap()
            .scope;

        let first = backfill_personal_task_scopes(store.as_ref()).await.unwrap();
        let second = backfill_personal_task_scopes(store.as_ref()).await.unwrap();

        assert_eq!(first, 0);
        assert_eq!(second, 0);
        let after = store
            .get_personal_task_scope_for_user(&user_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.id, existing.id);
    }

    #[tokio::test]
    async fn backfill_personal_task_scopes_repairs_prefix_set_scope_missing_partial_state() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user_id = create_user(&store, "partial-state").await;
        // Simulate the documented partial state: prefix committed, but scope
        // materialisation failed before creating a task_scopes row.
        store.set_user_prefix(&user_id, "PARTIAL").await.unwrap();

        let count = backfill_personal_task_scopes(store.as_ref()).await.unwrap();
        let second = backfill_personal_task_scopes(store.as_ref()).await.unwrap();

        assert_eq!(count, 1);
        assert_eq!(second, 0);
        let scope = store
            .get_personal_task_scope_for_user(&user_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope.key_prefix, "PARTIAL");
    }

    #[tokio::test]
    async fn backfill_personal_task_scopes_counts_prefixed_scopeless_users() {
        let fixture = migrated_store().await;
        let store = fixture.store.clone();
        let user_a = create_user(&store, "scope-a").await;
        let user_b = create_user(&store, "scope-b").await;
        let user_without_prefix = create_user(&store, "scope-none").await;
        store.set_user_prefix(&user_a, "SCOPEA").await.unwrap();
        store.set_user_prefix(&user_b, "SCOPEB").await.unwrap();

        let count = backfill_personal_task_scopes(store.as_ref()).await.unwrap();
        let second = backfill_personal_task_scopes(store.as_ref()).await.unwrap();

        assert_eq!(count, 2);
        assert_eq!(second, 0);
        assert_eq!(
            store
                .get_personal_task_scope_for_user(&user_a)
                .await
                .unwrap()
                .unwrap()
                .key_prefix,
            "SCOPEA"
        );
        assert_eq!(
            store
                .get_personal_task_scope_for_user(&user_b)
                .await
                .unwrap()
                .unwrap()
                .key_prefix,
            "SCOPEB"
        );
        assert!(store
            .get_personal_task_scope_for_user(&user_without_prefix)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn result_always_matches_format_regex() {
        // Spot-check across cases: ^[A-Z][A-Z0-9]{0,9}$
        let cases = [
            ("alice", Uuid::nil(), no_existing()),
            ("123abc", Uuid::nil(), no_existing()),
            ("", Uuid::nil(), no_existing()),
            ("alicewasinwonderland", Uuid::nil(), no_existing()),
            ("a-li.ce_w📚", Uuid::nil(), no_existing()),
        ];
        for (username, uuid, taken) in cases {
            let p = derive_prefix(username, &uuid, &taken);
            assert!(!p.is_empty());
            assert!(p.len() <= 10, "{p} too long");
            let mut chars = p.chars();
            let first = chars.next().unwrap();
            assert!(first.is_ascii_uppercase(), "{p} bad first char");
            for c in chars {
                assert!(
                    c.is_ascii_uppercase() || c.is_ascii_digit(),
                    "{p} has invalid char {c}"
                );
            }
        }
    }
}
