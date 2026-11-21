//! Persistent request admission and canonical dispatch references.
//!
//! No TTL, receipt cap or session deletion touches these claims: forgetting an
//! unresolved external outcome could permit a duplicate worker submission.

use super::SqliteSessionBackend;
use anyhow::{Context, Result, bail, ensure};
use reiclaw_api::delegation_request::{
    DelegationOrigin, DelegationRequestBinding, DelegationRequestClaim,
};
use rusqlite::{Connection, TransactionBehavior, params};

// Existing scoped rows stay untouched. Schema discovery and migration share
// the write lock so independently opened handles cannot race the ALTER.
pub(super) fn migrate_route_provenance(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_route: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'route_digest'",
        [],
        |row| row.get(0),
    ).context("inspecting delegation route provenance schema")?;
    if !has_route {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN route_digest TEXT",
            [],
        )
        .context("adding delegation route provenance")?;
    }
    tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_session_delegations_request ON session_delegations(request_id)",
        [],
    ).context("indexing delegation request lookup")?;
    let has_notified: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'notified_outcome'",
        [],
        |row| row.get(0),
    ).context("inspecting delegation notification schema")?;
    if !has_notified {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN notified_outcome TEXT",
            [],
        )
        .context("adding delegation notification schema")?;
    }
    let has_digest: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'terminal_digest'",
        [], |row| row.get(0),
    ).context("inspecting terminal digest receipt schema")?;
    if !has_digest {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN terminal_digest TEXT",
            [],
        )
        .context("adding terminal digest receipt schema")?;
    }
    let has_proxy_continuations: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'proxy_continuations'",
            [],
            |row| row.get(0),
        )
        .context("inspecting delegation proxy continuation count schema")?;
    if !has_proxy_continuations {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN proxy_continuations INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .context("adding delegation proxy continuation count schema")?;
    }
    let has_last_proxy_revision: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'last_proxy_revision'",
            [],
            |row| row.get(0),
        )
        .context("inspecting delegation proxy revision schema")?;
    if !has_last_proxy_revision {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN last_proxy_revision INTEGER",
            [],
        )
        .context("adding delegation proxy revision schema")?;
    }
    let has_proxy_state: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'proxy_continuation_state'",
        [],
        |row| row.get(0),
    ).context("inspecting delegation proxy continuation state schema")?;
    if !has_proxy_state {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN proxy_continuation_state TEXT NOT NULL DEFAULT 'idle'",
            [],
        ).context("adding delegation proxy continuation state schema")?;
        // The old writer counted both accepted and unknown submissions. Its
        // count/revision cannot prove a receipt, so upgrading must not turn
        // that history into permission to submit again.
        tx.execute(
            "UPDATE session_delegations SET proxy_continuation_state = 'unknown:legacy'
             WHERE proxy_continuations != 0 OR last_proxy_revision IS NOT NULL",
            [],
        )
        .context("preserving unresolved legacy proxy continuations")?;
    }
    tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_session_delegations_dispatch ON session_delegations(dispatch_id)",
        [],
    ).context("indexing canonical delegation dispatch lookup")?;
    super::delegation_origin::migrate(&tx)?;
    tx.commit()
        .context("committing delegation route provenance schema")
}

/// One dispatch-binding row for the (#63) unattended observer.
/// `agent_alias` is the OWNING alias: legacy scopes (`alias:<64-hex>`)
/// are parsed down to the alias so agent-eligibility lookups match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBindingRow {
    pub rowid: i64,
    pub agent_alias: String,
    pub request_id: String,
    pub dispatch_id: String,
    pub route_digest: Option<String>,
    pub notified_outcome: Option<String>,
    /// Independent local Episode receipt; NULL also covers pre-upgrade bindings.
    pub terminal_digest: Option<String>,
    pub proxy_continuations: u8,
    pub last_proxy_revision: Option<u64>,
    pub proxy_continuation_state: ProxyContinuationState,
    pub origin: Option<DelegationOrigin>,
}

/// Durable disposition of the latest automatic continuation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyContinuationState {
    Idle,
    Reserved,
    Unknown,
    Settled,
    Unsent,
}

/// Only a freshly reserved attempt authorizes one external submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyContinuationAdmission {
    Reserved(ProxyContinuationReservation),
    Unresolved,
    CapExceeded { prior: u64, max: u8 },
    AlreadyRecorded { last_revision: u64 },
    StaleRevision { last_revision: u64 },
    SourceUnavailable,
}

/// Opaque local ownership of one reservation; never an external request ID.
/// A new nonce on an unsent retry prevents an old completion from settling or
/// refunding a later attempt at the same canonical revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyContinuationReservation {
    rowid: i64,
    scope: String,
    route_digest: Option<String>,
    dispatch_id: String,
    revision: u64,
    nonce: String,
    owner_count: i64,
    round: u8,
}

impl ProxyContinuationReservation {
    #[must_use]
    pub const fn round(&self) -> u8 {
        self.round
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

fn decode_proxy_state(
    raw: &str,
    count: u64,
    revision: Option<u64>,
) -> Result<ProxyContinuationState> {
    if raw == "idle" {
        ensure!(
            count == 0 && revision.is_none(),
            "idle proxy continuation has attempt history"
        );
        return Ok(ProxyContinuationState::Idle);
    }
    if raw == "unknown:legacy" {
        ensure!(
            count != 0 || revision.is_some(),
            "legacy proxy continuation has no attempt history"
        );
        return Ok(ProxyContinuationState::Unknown);
    }
    let (state, nonce) = raw
        .split_once(':')
        .context("invalid proxy continuation state")?;
    uuid::Uuid::parse_str(nonce).context("invalid proxy continuation attempt nonce")?;
    ensure!(
        revision.is_some(),
        "proxy continuation attempt has no revision"
    );
    let state = match state {
        "reserved" => ProxyContinuationState::Reserved,
        "unknown" => ProxyContinuationState::Unknown,
        "settled" => ProxyContinuationState::Settled,
        "unsent" => ProxyContinuationState::Unsent,
        _ => bail!("invalid proxy continuation disposition"),
    };
    ensure!(
        count != 0 || state == ProxyContinuationState::Unsent,
        "proxy continuation attempt has no reserved count"
    );
    Ok(state)
}

struct ContinuationBinding {
    rowid: i64,
    scope: String,
    agent_alias: String,
    request_id: String,
    route_digest: Option<String>,
    count: u64,
    revision: Option<u64>,
    state: ProxyContinuationState,
}

fn continuation_bindings(
    conn: &Connection,
    binding: &DispatchBindingRow,
) -> Result<Vec<ContinuationBinding>> {
    let expected_route = binding
        .route_digest
        .as_deref()
        .filter(|route| !route.is_empty())
        .context("proxy continuation requires immutable route provenance")?;
    let mut query = conn.prepare(
        "SELECT rowid, agent_alias, request_id, route_digest,
                proxy_continuations, last_proxy_revision, proxy_continuation_state
         FROM session_delegations WHERE dispatch_id = ?1 ORDER BY rowid",
    )?;
    let mut rows = query.query([&binding.dispatch_id])?;
    let mut group = Vec::new();
    while let Some(row) = rows.next()? {
        let scope: String = row.get(1)?;
        let stored_route: Option<String> = row.get(3)?;
        let legacy = scope.rsplit_once(':').filter(|(_, route)| {
            route.len() == 64
                && route
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        let (agent_alias, route) = if let Some((alias, legacy)) = legacy {
            ensure!(
                stored_route.as_deref().is_none_or(|route| route == legacy),
                "legacy delegation route provenance conflicts"
            );
            (alias, legacy)
        } else {
            // No route fact means an old attempt cannot safely be excluded
            // from this dispatch's budget, even by claiming a new alias.
            (
                scope.as_str(),
                stored_route
                    .as_deref()
                    .filter(|route| !route.is_empty())
                    .context("canonical dispatch has an unscoped legacy claim")?,
            )
        };
        if route != expected_route {
            continue;
        }
        let count =
            u64::try_from(row.get::<_, i64>(4)?).context("negative proxy continuation count")?;
        let revision = row
            .get::<_, Option<i64>>(5)?
            .map(u64::try_from)
            .transpose()
            .context("negative proxy continuation revision")?;
        let state = decode_proxy_state(&row.get::<_, String>(6)?, count, revision)?;
        group.push(ContinuationBinding {
            rowid: row.get(0)?,
            agent_alias: agent_alias.to_string(),
            scope,
            request_id: row.get(2)?,
            route_digest: stored_route,
            count,
            revision,
            state,
        });
    }
    ensure!(
        group.iter().any(|row| row.rowid == binding.rowid
            && row.request_id == binding.request_id
            && row.agent_alias == binding.agent_alias),
        "proxy continuation binding no longer matches its canonical identity"
    );
    Ok(group)
}

/// Bounded recovery projection of the existing claims, including uncertain
/// submissions. No execution state is inferred from this local reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentDelegationBindingRow {
    pub request_id: String,
    pub dispatch_id: Option<String>,
    pub route_digest: Option<String>,
}

fn legacy_route<'a>(scope: &'a str, agent_alias: &str) -> Option<&'a str> {
    let suffix = scope.strip_prefix(agent_alias)?.strip_prefix(':')?;
    (suffix.len() == 64
        && suffix
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    .then_some(suffix)
}

// Query by request ID, then compare full keys. LIKE would confuse underscores
// in real aliases with wildcard matches and could seize another agent's claim.
// A legacy key carries the route fact already persisted by the old writer; no
// row is adopted, rewritten, merged or deleted during upgrade/read/replay.
fn find_binding(
    conn: &Connection,
    agent_alias: &str,
    request_id: &str,
) -> Result<Option<DelegationRequestBinding>> {
    let mut query = conn.prepare(
        "SELECT agent_alias, request_digest, dispatch_id, route_digest, origin_context
         FROM session_delegations WHERE request_id = ?1",
    )?;
    let rows = query.query_map([request_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut binding = None;
    for row in rows {
        let (scope, request_digest, dispatch_id, route_digest, origin) = row?;
        let route_digest = if scope == agent_alias {
            route_digest
        } else if route_digest.is_none() {
            let Some(route) = legacy_route(&scope, agent_alias) else {
                continue;
            };
            Some(route.to_string())
        } else {
            continue;
        };
        if binding.is_some() {
            bail!(
                "multiple delegation claims exist for this agent/request; explicit owner reconciliation is required"
            );
        }
        binding = Some(DelegationRequestBinding {
            request_digest,
            route_digest,
            dispatch_id,
            origin: origin.as_deref().map(serde_json::from_str).transpose()?,
        });
    }
    Ok(binding)
}

impl SqliteSessionBackend {
    /// Release only the newly created stable claim after proving no start was
    /// sent. Legacy claims are never released by a new admission attempt.
    pub fn release_unsent_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM session_delegations
             WHERE agent_alias = ?1 AND request_id = ?2 AND request_digest = ?3
               AND route_digest = ?4 AND dispatch_id IS NULL",
                params![agent_alias, request_id, request_digest, route_digest],
            )
            .context("releasing a proven unsent delegation request")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(deleted == 1)
    }

    /// Claim the true agent/request key before first external submission. The
    /// immutable route fact is created here, never used as part of that key.
    /// An IMMEDIATE transaction also arbitrates preserved legacy scoped rows
    /// before insertion, across independently opened handles/processes.
    pub fn claim_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
    ) -> Result<DelegationRequestClaim> {
        self.claim_delegation_request_with_origin(
            agent_alias,
            request_id,
            request_digest,
            route_digest,
            None,
        )
    }

    /// Persist host-admitted origin with the initial claim, before any start
    /// can be sent. A replay cannot acquire, replace or revive a fact grant.
    pub fn claim_delegation_request_with_origin(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
        origin: Option<&DelegationOrigin>,
    ) -> Result<DelegationRequestClaim> {
        if route_digest.is_empty() {
            bail!("delegation route provenance must not be empty");
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claim = if let Some(binding) = find_binding(&tx, agent_alias, request_id)? {
            if binding.request_digest != request_digest
                || binding.route_digest.as_deref() != Some(route_digest)
                || binding.origin.as_ref() != origin
            {
                DelegationRequestClaim::Conflict
            } else {
                DelegationRequestClaim::Existing {
                    dispatch_id: binding.dispatch_id,
                }
            }
        } else {
            if let Some(origin) = origin {
                super::delegation_origin::validate_admission(&tx, agent_alias, origin)?;
            }
            let origin = origin.map(serde_json::to_string).transpose()?;
            tx.execute(
                "INSERT INTO session_delegations
                    (agent_alias, request_id, request_digest, route_digest, dispatch_id, origin_context)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                params![agent_alias, request_id, request_digest, route_digest, origin],
            )
            .context("claiming a delegation request")?;
            DelegationRequestClaim::Created
        };
        tx.commit()
            .context("committing delegation request admission")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(claim)
    }

    /// Bind only the matching stable claim. Neither route provenance nor an
    /// existing known dispatch can be overwritten by another response.
    pub fn bind_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
        dispatch_id: &str,
    ) -> Result<()> {
        if dispatch_id.is_empty() {
            bail!("delegation dispatch ID must not be empty");
        }
        let conn = self.conn.lock();
        let updated = conn
            .execute(
                "UPDATE session_delegations SET dispatch_id = ?5
             WHERE agent_alias = ?1 AND request_id = ?2 AND request_digest = ?3
               AND route_digest = ?4 AND (dispatch_id IS NULL OR dispatch_id = ?5)",
                params![
                    agent_alias,
                    request_id,
                    request_digest,
                    route_digest,
                    dispatch_id
                ],
            )
            .context("binding a delegation request to its canonical dispatch")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        if updated != 1 {
            bail!(
                "delegation binding rejected: missing claim or conflicting payload/route/dispatch"
            );
        }
        Ok(())
    }

    /// The most recent claims owned by one exact agent, newest first. The one
    /// extra row lets a caller report truncation without creating a cursor store.
    pub fn recent_delegation_bindings(
        &self,
        agent_alias: &str,
        limit: usize,
    ) -> Result<Vec<RecentDelegationBindingRow>> {
        if !(1..=21).contains(&limit) {
            bail!("recent delegation limit must be between 1 and 21");
        }
        let conn = self.conn.lock();
        let mut query = conn.prepare(
            "SELECT agent_alias, request_id, dispatch_id, route_digest
             FROM session_delegations
             WHERE agent_alias = ?1 OR (
                route_digest IS NULL
                AND length(agent_alias) = length(?1) + 65
                AND substr(agent_alias, 1, length(?1) + 1) = ?1 || ':'
                AND substr(agent_alias, length(?1) + 2) NOT GLOB '*[^0-9a-f]*'
             )
             ORDER BY rowid DESC LIMIT ?2",
        )?;
        query
            .query_map(params![agent_alias, limit as i64], |row| {
                let scope: String = row.get(0)?;
                let route: Option<String> = row.get(3)?;
                Ok(RecentDelegationBindingRow {
                    request_id: row.get(1)?,
                    dispatch_id: row.get(2)?,
                    route_digest: route
                        .or_else(|| legacy_route(&scope, agent_alias).map(str::to_owned)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// One page of dispatch bindings strictly after `after_rowid`, ascending.
    /// Pagination is by the implicit rowid; the observer rotates its cursor
    /// across passes so a bounded page can never permanently starve later
    /// rows (delegation claims are permanent and never TTL-evicted).
    pub fn list_dispatch_bindings_page(
        &self,
        after_rowid: i64,
        limit: usize,
    ) -> Result<Vec<DispatchBindingRow>> {
        let conn = self.conn.lock();
        let mut query = conn.prepare(
            "SELECT rowid, agent_alias, request_id, dispatch_id, route_digest, notified_outcome,
                    proxy_continuations, last_proxy_revision, proxy_continuation_state, origin_context, terminal_digest
             FROM session_delegations
             WHERE dispatch_id IS NOT NULL AND rowid > ?1
             ORDER BY rowid LIMIT ?2",
        )?;
        let rows = query
            .query_map(params![after_rowid, limit as i64], |row| {
                let scope: String = row.get(1)?;
                let stored_digest: Option<String> = row.get(4)?;
                // Legacy route scope (`alias:<64-hex>`): report the owner AND
                // recover the route fact from the suffix — legacy rows carry
                // a NULL route_digest column (the same recovery find_binding
                // performs for the interactive tools).
                let legacy = scope.split_once(':').filter(|(_, suffix)| {
                    suffix.len() == 64
                        && suffix
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                });
                let (owner, route_digest) = match legacy {
                    Some((alias, suffix)) => (
                        alias.to_string(),
                        stored_digest.or_else(|| Some(suffix.to_string())),
                    ),
                    None => (scope.clone(), stored_digest),
                };
                let raw_count: i64 = row.get(6)?;
                let proxy_continuations = u8::try_from(raw_count).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Integer,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid proxy_continuations value: {raw_count}"),
                        )),
                    )
                })?;
                let raw_rev: Option<i64> = row.get(7)?;
                let last_proxy_revision = raw_rev
                    .map(|rev| {
                        u64::try_from(rev).map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                7,
                                rusqlite::types::Type::Integer,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("invalid last_proxy_revision value: {rev}"),
                                )),
                            )
                        })
                    })
                    .transpose()?;
                let proxy_continuation_state = decode_proxy_state(
                    &row.get::<_, String>(8)?,
                    u64::from(proxy_continuations),
                    last_proxy_revision,
                )
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            error.to_string(),
                        )),
                    )
                })?;
                Ok(DispatchBindingRow {
                    rowid: row.get(0)?,
                    agent_alias: owner,
                    request_id: row.get(2)?,
                    dispatch_id: row.get(3)?,
                    route_digest,
                    notified_outcome: row.get(5)?,
                    terminal_digest: row.get(10)?,
                    proxy_continuations,
                    last_proxy_revision,
                    proxy_continuation_state,
                    origin: row
                        .get::<_, Option<String>>(9)?
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                9,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                })
            })
            .context("listing delegation dispatch bindings")?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The current notified-marker history of one binding (by rowid), or
    /// `None` when the row carries no marker. Read errors propagate — a
    /// lost-history overwrite is worse than a skipped flush.
    pub fn binding_notified_marker(&self, rowid: i64) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT notified_outcome FROM session_delegations WHERE rowid = ?1",
            params![rowid],
            // SQL NULL (never notified) must read as None, not error.
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .context("reading a delegation notification marker")
        .map(|value| value.flatten())
    }

    /// The highest binding rowid, or 0 for an empty table. The observer uses
    /// this at sweep start to distinguish churn (rows appearing beyond the
    /// sweep's original extent) from a finite table it is simply walking.
    pub fn max_binding_rowid(&self) -> Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM session_delegations",
            [],
            |row| row.get(0),
        )
        .context("reading the max delegation rowid")
    }

    /// Persist the outcome this binding's owner was already notified about.
    /// The marker survives outbox retention (the outbox may evict its dedup
    /// receipts; the delegation claim never expires), so a long-terminal run
    /// cannot be re-notified after retention cleanup.
    pub fn mark_binding_notified(&self, rowid: i64, outcome: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE session_delegations SET notified_outcome = ?2 WHERE rowid = ?1",
            params![rowid, outcome],
        )
        .context("marking delegation notification")?;
        Ok(())
    }

    /// Record successful local digestion independently of destination notification.
    /// `None` means an Episode exists; a revision means that canonical result had
    /// no digestible body. Neither receipt authorizes an external mutation.
    pub fn record_terminal_digest(
        &self,
        binding: &DispatchBindingRow,
        empty_revision: Option<u64>,
    ) -> Result<()> {
        let route = binding
            .route_digest
            .as_deref()
            .context("terminal digest requires route provenance")?;
        let receipt =
            empty_revision.map_or_else(|| "stored".to_string(), |rev| format!("empty:{rev}"));
        let conn = self.conn.lock();
        let changed = conn
            .execute(
                "UPDATE session_delegations
             SET terminal_digest = CASE WHEN terminal_digest = 'stored' THEN terminal_digest ELSE ?1 END
             WHERE rowid = ?2 AND request_id = ?3 AND dispatch_id = ?4
               AND ((agent_alias = ?5 AND route_digest = ?6)
                 OR (agent_alias = ?5 || ':' || ?6 AND route_digest IS NULL))",
                params![
                    receipt,
                    binding.rowid,
                    binding.request_id,
                    binding.dispatch_id,
                    binding.agent_alias,
                    route
                ],
            )
            .context("recording terminal digest receipt")?;
        ensure!(
            changed == 1,
            "terminal digest binding changed before receipt commit"
        );
        Ok(())
    }

    /// Reserve one automatic continuation before sending. The canonical route
    /// and dispatch own the budget: additional request IDs, aliases sharing a
    /// Tachi identity, and preserved legacy scopes cannot reset it.
    ///
    /// `max_attempts` may narrow, never exceed, the three-attempt contract. Any
    /// unresolved attempt blocks further reservations regardless of revision.
    pub fn reserve_proxy_continuation(
        &self,
        binding: &DispatchBindingRow,
        expected_revision: u64,
        max_attempts: u8,
    ) -> Result<ProxyContinuationAdmission> {
        ensure!(
            (1..=3).contains(&max_attempts),
            "proxy continuation limit must be between one and three"
        );
        let rev_i64 = i64::try_from(expected_revision)
            .context("proxy continuation revision exceeds SQLite INTEGER range")?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group = continuation_bindings(&tx, binding)?;
        if group.iter().any(|row| {
            matches!(
                row.state,
                ProxyContinuationState::Reserved | ProxyContinuationState::Unknown
            )
        }) {
            return Ok(ProxyContinuationAdmission::Unresolved);
        }
        let prior = group.iter().try_fold(0_u64, |total, row| {
            total
                .checked_add(row.count)
                .context("proxy continuation aggregate count overflow")
        })?;
        if prior >= u64::from(max_attempts) {
            return Ok(ProxyContinuationAdmission::CapExceeded {
                prior,
                max: max_attempts,
            });
        }
        if let Some(last_revision) = group.iter().filter_map(|row| row.revision).max() {
            if expected_revision < last_revision {
                return Ok(ProxyContinuationAdmission::StaleRevision { last_revision });
            }
            if expected_revision == last_revision
                && group.iter().any(|row| {
                    row.revision == Some(last_revision)
                        && row.state != ProxyContinuationState::Unsent
                })
            {
                return Ok(ProxyContinuationAdmission::AlreadyRecorded { last_revision });
            }
        }
        if !super::delegation_origin::is_current(&tx, binding)? {
            return Ok(ProxyContinuationAdmission::SourceUnavailable);
        }
        // Claims are permanent. The first row remains the local write owner;
        // all rows still participate in the aggregate above, including later
        // aliases/requests and old per-request counters.
        let owner = group
            .first()
            .context("canonical dispatch has no continuation owner")?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let owner_count = i64::try_from(owner.count + 1)
            .context("proxy continuation count exceeds SQLite INTEGER range")?;
        let reservation = ProxyContinuationReservation {
            rowid: owner.rowid,
            scope: owner.scope.clone(),
            route_digest: owner.route_digest.clone(),
            dispatch_id: binding.dispatch_id.clone(),
            revision: expected_revision,
            nonce,
            owner_count,
            round: u8::try_from(prior + 1).context("proxy continuation round exceeds its limit")?,
        };
        let updated = tx
            .execute(
                "UPDATE session_delegations
             SET proxy_continuations = ?2, last_proxy_revision = ?3,
                 proxy_continuation_state = ?4
             WHERE rowid = ?1",
                params![
                    owner.rowid,
                    owner_count,
                    rev_i64,
                    format!("reserved:{}", reservation.nonce)
                ],
            )
            .context("reserving proxy continuation before transmission")?;
        ensure!(
            updated == 1,
            "proxy continuation reservation was not persisted"
        );
        tx.commit()
            .context("committing proxy continuation reservation")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(ProxyContinuationAdmission::Reserved(reservation))
    }

    /// Record the trusted receipt for this exact attempt, keeping its budget.
    /// A newer status revision alone is never evidence to settle an attempt.
    pub fn settle_proxy_continuation(
        &self,
        reservation: &ProxyContinuationReservation,
    ) -> Result<bool> {
        self.finish_proxy_continuation(reservation, ProxyContinuationState::Settled)
    }

    /// Preserve a possibly transmitted attempt for explicit reconciliation.
    pub fn mark_proxy_continuation_unknown(
        &self,
        reservation: &ProxyContinuationReservation,
    ) -> Result<bool> {
        self.finish_proxy_continuation(reservation, ProxyContinuationState::Unknown)
    }

    /// Refund only a still-reserved attempt with proof that no continuation
    /// was sent. Its revision remains a high-water mark; a same-revision retry
    /// receives a new nonce. Unknown attempts cannot be refunded here.
    pub fn release_unsent_proxy_continuation(
        &self,
        reservation: &ProxyContinuationReservation,
    ) -> Result<bool> {
        self.finish_proxy_continuation(reservation, ProxyContinuationState::Unsent)
    }

    fn finish_proxy_continuation(
        &self,
        reservation: &ProxyContinuationReservation,
        next: ProxyContinuationState,
    ) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let (prefix, refund) = match next {
            ProxyContinuationState::Settled => ("settled", false),
            ProxyContinuationState::Unknown => ("unknown", false),
            ProxyContinuationState::Unsent => ("unsent", true),
            _ => bail!("invalid proxy continuation completion"),
        };
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, i64)> = tx
            .query_row(
                "SELECT proxy_continuation_state, proxy_continuations FROM session_delegations
             WHERE rowid = ?1 AND agent_alias = ?2 AND route_digest IS ?3
               AND dispatch_id = ?4 AND last_proxy_revision = ?5",
                params![
                    reservation.rowid,
                    reservation.scope,
                    reservation.route_digest,
                    reservation.dispatch_id,
                    i64::try_from(reservation.revision)?
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .context("reading owned proxy continuation reservation")?;
        let Some((state, count)) = current else {
            return Ok(false);
        };
        let target = format!("{prefix}:{}", reservation.nonce);
        let target_count = reservation.owner_count - if refund { 1 } else { 0 };
        if state == target && count == target_count {
            return Ok(true);
        }
        let reserved = format!("reserved:{}", reservation.nonce);
        let unknown = format!("unknown:{}", reservation.nonce);
        if count != reservation.owner_count
            || !(state == reserved || (next == ProxyContinuationState::Settled && state == unknown))
        {
            return Ok(false);
        }
        let updated = tx.execute(
            "UPDATE session_delegations SET proxy_continuation_state = ?2, proxy_continuations = ?3
             WHERE rowid = ?1",
            params![reservation.rowid, target, target_count],
        ).context("persisting proxy continuation disposition")?;
        ensure!(
            updated == 1,
            "proxy continuation disposition was not persisted"
        );
        tx.commit()
            .context("committing proxy continuation disposition")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(true)
    }

    /// Resolve a stable or uniquely preserved legacy reference. The caller
    /// must compare immutable route provenance before using the dispatch ID.
    pub fn read_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
    ) -> Result<Option<DelegationRequestBinding>> {
        find_binding(&self.conn.lock(), agent_alias, request_id)
            .context("reading a delegation request binding")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_backend::SessionBackend;
    use reiclaw_api::model_provider::ChatMessage;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    #[test]
    fn recent_claims_are_bounded_exact_agent_owned_and_include_uncertain_legacy_rows() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        for i in 0..25 {
            store
                .claim_delegation_request("agent_%", &format!("r{i}"), "digest", "route")
                .unwrap();
        }
        store
            .bind_delegation_request("agent_%", "r24", "digest", "route", "d24")
            .unwrap();
        store
            .claim_delegation_request("agent_AX", "private", "digest", "route")
            .unwrap();
        let legacy_route = "a".repeat(64);
        store.conn.lock().execute(
            "INSERT INTO session_delegations (agent_alias, request_id, request_digest, dispatch_id)
             VALUES (?1, 'legacy', 'digest', NULL)",
            [format!("agent_%:{legacy_route}")],
        ).unwrap();
        let rows = store.recent_delegation_bindings("agent_%", 21).unwrap();
        assert_eq!(rows.len(), 21);
        assert_eq!(rows[0].request_id, "legacy");
        assert_eq!(rows[0].route_digest.as_deref(), Some(legacy_route.as_str()));
        assert!(rows[0].dispatch_id.is_none());
        assert_eq!(rows[1].request_id, "r24");
        assert_eq!(rows[1].dispatch_id.as_deref(), Some("d24"));
        assert!(!rows.iter().any(|row| row.request_id == "private"));
        assert!(store.recent_delegation_bindings("agent_%", 0).is_err());
        assert!(store.recent_delegation_bindings("agent_%", 22).is_err());
    }

    #[test]
    fn unsent_release_matches_scope_digest_and_never_removes_known_dispatch() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        store
            .claim_delegation_request("a", "r", "digest", "route")
            .unwrap();
        assert!(
            !store
                .release_unsent_delegation_request("b", "r", "digest", "route")
                .unwrap()
        );
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "other", "route")
                .unwrap()
        );
        assert!(
            store
                .release_unsent_delegation_request("a", "r", "digest", "route")
                .unwrap()
        );
        assert_eq!(store.read_delegation_request("a", "r").unwrap(), None);
        assert_eq!(
            store
                .claim_delegation_request("a", "r", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Created
        );
        store
            .bind_delegation_request("a", "r", "digest", "route", "d")
            .unwrap();
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "digest", "route")
                .unwrap()
        );
        assert_eq!(
            store
                .read_delegation_request("a", "r")
                .unwrap()
                .unwrap()
                .dispatch_id
                .as_deref(),
            Some("d")
        );
    }

    #[test]
    fn delegation_route_is_immutable_across_claim_bind_release_and_reopen() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            store
                .claim_delegation_request("a", "r", "digest", "route-a")
                .unwrap(),
            DelegationRequestClaim::Created
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "r", "digest", "route-b")
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "digest", "route-b")
                .unwrap()
        );
        assert!(
            store
                .bind_delegation_request("a", "r", "digest", "route-b", "wrong")
                .is_err()
        );
        store
            .bind_delegation_request("a", "r", "digest", "route-a", "right")
            .unwrap();
        drop(store);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        let binding = reopened.read_delegation_request("a", "r").unwrap().unwrap();
        assert_eq!(binding.route_digest.as_deref(), Some("route-a"));
        assert_eq!(binding.dispatch_id.as_deref(), Some("right"));
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r", "digest", "route-b")
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
    }

    #[test]
    fn legacy_route_parser_requires_the_complete_alias_and_exact_lowercase_hash() {
        let hash = "a".repeat(64);
        assert_eq!(
            legacy_route(&format!("a_b:{hash}"), "a_b"),
            Some(hash.as_str())
        );
        for scope in [
            format!("axb:{hash}"),
            format!("a_b_extra:{hash}"),
            format!("a_b:{}", "A".repeat(64)),
            format!("a_b:{hash}0"),
            format!("a_b:{hash}:other"),
        ] {
            assert_eq!(legacy_route(&scope, "a_b"), None, "{scope}");
        }
    }

    #[test]
    fn legacy_scoped_claims_survive_upgrade_without_adoption_or_wildcard_collision() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let route = "a".repeat(64);
        let other_route = "b".repeat(64);
        {
            let conn = store.conn.lock();
            conn.execute_batch(
                "DROP TRIGGER delegation_origin_session_deleted;
                 DROP TRIGGER delegation_origin_owner_changed;
                 DROP TABLE session_delegations;
                CREATE TABLE session_delegations (
                    agent_alias TEXT NOT NULL, request_id TEXT NOT NULL,
                    request_digest TEXT NOT NULL, dispatch_id TEXT,
                    PRIMARY KEY(agent_alias, request_id));",
            )
            .unwrap();
            for (alias, request, digest, dispatch) in [
                (format!("a_b:{route}"), "pending", "ours", None),
                (
                    format!("axb:{route}"),
                    "pending",
                    "foreign",
                    Some("foreign-dispatch"),
                ),
                (
                    format!("a_b:{route}"),
                    "bound",
                    "ours",
                    Some("old-dispatch"),
                ),
                (format!("a_b:{route}"), "multiple", "ours", None),
                (
                    format!("a_b:{other_route}"),
                    "multiple",
                    "other",
                    Some("other-dispatch"),
                ),
                ("unscoped".to_string(), "pending", "unknown-route", None),
            ] {
                conn.execute(
                    "INSERT INTO session_delegations VALUES(?1,?2,?3,?4)",
                    params![alias, request, digest, dispatch],
                )
                .unwrap();
            }
        }
        drop(store);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        for (request, dispatch) in [("pending", None), ("bound", Some("old-dispatch"))] {
            assert_eq!(
                reopened
                    .claim_delegation_request("a_b", request, "ours", &route)
                    .unwrap(),
                DelegationRequestClaim::Existing {
                    dispatch_id: dispatch.map(str::to_string)
                }
            );
            assert_eq!(
                reopened
                    .claim_delegation_request("a_b", request, "ours", &other_route)
                    .unwrap(),
                DelegationRequestClaim::Conflict
            );
            let binding = reopened
                .read_delegation_request("a_b", request)
                .unwrap()
                .unwrap();
            assert_eq!(binding.route_digest.as_deref(), Some(route.as_str()));
            assert_eq!(binding.dispatch_id.as_deref(), dispatch);
            assert!(
                !reopened
                    .release_unsent_delegation_request("a_b", request, "ours", &route)
                    .unwrap()
            );
        }
        assert_eq!(
            reopened
                .read_delegation_request("axb", "pending")
                .unwrap()
                .unwrap()
                .dispatch_id
                .as_deref(),
            Some("foreign-dispatch")
        );
        assert!(
            reopened
                .claim_delegation_request("a_b", "multiple", "ours", &route)
                .is_err()
        );
        assert!(reopened.read_delegation_request("a_b", "multiple").is_err());
        assert_eq!(
            reopened
                .claim_delegation_request("unscoped", "pending", "unknown-route", &route)
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
        let conn = reopened.conn.lock();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM session_delegations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            rows, 6,
            "all foreign/ambiguous/known legacy claims remain untouched"
        );
        let new_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_delegations WHERE route_digest IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            new_rows, 0,
            "upgrade/replay must not adopt or copy a legacy row"
        );
    }

    #[test]
    fn concurrent_route_changes_cannot_create_two_stable_claims() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [(first, "route-a"), (second, "route-b")]
            .into_iter()
            .map(|(store, route)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_delegation_request("a", "r", "digest", route)
                        .unwrap()
                })
            })
            .collect();
        let claims: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Created)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Conflict)
                .count(),
            1
        );
    }

    #[test]
    fn delegation_claim_distinguishes_duplicate_and_conflicting_digest() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(store.read_delegation_request("a", "r1").unwrap(), None);
        assert_eq!(
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Created
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "r1", "other", "route")
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
        assert_eq!(
            store.read_delegation_request("a", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest".into(),
                route_digest: Some("route".into()),
                dispatch_id: None,
                origin: None,
            })
        );
    }

    #[test]
    fn delegation_unresolved_claim_survives_reopen_without_resubmission() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            assert_eq!(
                store
                    .claim_delegation_request("a", "r1", "digest", "route")
                    .unwrap(),
                DelegationRequestClaim::Created
            );
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
    }

    #[test]
    fn delegation_known_dispatch_survives_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap();
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d1")
                .unwrap();
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened.read_delegation_request("a", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest".into(),
                route_digest: Some("route".into()),
                dispatch_id: Some("d1".into()),
                origin: None,
            })
        );
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing {
                dispatch_id: Some("d1".into())
            }
        );
    }

    #[test]
    fn delegation_request_ids_are_agent_scoped() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        for (agent, digest) in [("a", "digest-a"), ("b", "digest-b")] {
            assert_eq!(
                store
                    .claim_delegation_request(agent, "r1", digest, "route")
                    .unwrap(),
                DelegationRequestClaim::Created
            );
        }
        store
            .bind_delegation_request("a", "r1", "digest-a", "route", "d1")
            .unwrap();
        assert_eq!(
            store.read_delegation_request("b", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest-b".into(),
                route_digest: Some("route".into()),
                dispatch_id: None,
                origin: None,
            })
        );
    }

    #[test]
    fn delegation_concurrent_handles_create_only_one_claim() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [first, second]
            .into_iter()
            .map(|store| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_delegation_request("a", "r1", "digest", "route")
                        .unwrap()
                })
            })
            .collect();
        let claims: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Created)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Existing { dispatch_id: None })
                .count(),
            1
        );
    }

    #[test]
    fn delegation_binding_is_digest_bound_and_rejects_conflicting_dispatch() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d1")
                .is_err()
        );
        store
            .claim_delegation_request("a", "r1", "digest", "route")
            .unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "other", "route", "d1")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "")
                .is_err()
        );
        store
            .bind_delegation_request("a", "r1", "digest", "route", "d1")
            .unwrap();
        store
            .bind_delegation_request("a", "r1", "digest", "route", "d1")
            .unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d2")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "other", "route", "d1")
                .is_err()
        );
        assert_eq!(
            store
                .read_delegation_request("a", "r1")
                .unwrap()
                .unwrap()
                .dispatch_id,
            Some("d1".into())
        );
    }

    #[test]
    fn delegation_concurrent_bind_cannot_overwrite_a_known_dispatch() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        first
            .claim_delegation_request("a", "r1", "digest", "route")
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [(first, "d1"), (second, "d2")]
            .into_iter()
            .map(|(store, id)| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    (
                        id,
                        store
                            .bind_delegation_request("a", "r1", "digest", "route", id)
                            .is_ok(),
                    )
                })
            })
            .collect();
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|(_, accepted)| *accepted).count(), 1);
        let winner = outcomes.iter().find(|(_, accepted)| *accepted).unwrap().0;
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .read_delegation_request("a", "r1")
                .unwrap()
                .unwrap()
                .dispatch_id
                .as_deref(),
            Some(winner)
        );
    }

    #[test]
    fn delegation_claims_survive_session_deletion_and_ttl_cleanup() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        for request in ["deleted", "expired"] {
            store
                .append(request, &ChatMessage::user("message"))
                .unwrap();
            store
                .claim_delegation_request("a", request, "digest", "route")
                .unwrap();
        }
        store
            .bind_delegation_request("a", "expired", "digest", "route", "d1")
            .unwrap();
        assert!(store.delete_session("deleted").unwrap());
        store.conn.lock().execute(
            "UPDATE session_metadata SET last_activity = '2000-01-01T00:00:00Z' WHERE session_key = 'expired'", []
        ).unwrap();
        assert_eq!(store.cleanup_stale(1).unwrap(), 1);
        assert_eq!(
            store
                .claim_delegation_request("a", "deleted", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "expired", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing {
                dispatch_id: Some("d1".into())
            }
        );
    }

    #[test]
    fn delegation_sqlite_errors_are_not_missing_or_created_results() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        store
            .conn
            .lock()
            .execute("DROP TABLE session_delegations", [])
            .unwrap();
        assert!(store.read_delegation_request("a", "r1").is_err());
        assert!(
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d1")
                .is_err()
        );
    }

    #[test]
    fn delegation_schema_is_added_to_an_existing_sessions_database() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            store
                .append("existing", &ChatMessage::user("retained message"))
                .unwrap();
            store
                .conn
                .lock()
                .execute_batch(
                    "DROP TRIGGER delegation_origin_session_deleted;
                                DROP TRIGGER delegation_origin_owner_changed;
                                DROP TABLE session_delegations;",
                )
                .unwrap();
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(reopened.load("existing")[0].content, "retained message");
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Created
        );
    }

    #[cfg(unix)]
    #[test]
    fn delegation_open_and_writes_harden_main_database_and_sidecars() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let paths: Vec<_> = ["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                let mut path = tmp
                    .path()
                    .join("sessions/sessions.db")
                    .as_os_str()
                    .to_os_string();
                path.push(suffix);
                std::path::PathBuf::from(path)
            })
            .filter(|path| path.exists())
            .collect();
        assert!(
            paths.len() > 1,
            "keep a live WAL sidecar for this regression"
        );
        for path in &paths {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for bind in [false, true] {
            for path in &paths {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).unwrap();
            }
            if bind {
                store
                    .bind_delegation_request("a", "r1", "digest", "route", "d1")
                    .unwrap();
            } else {
                store
                    .claim_delegation_request("a", "r1", "digest", "route")
                    .unwrap();
            }
            for path in &paths {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{} must be owner-only after the write",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn proxy_continuation_migration_is_idempotent_and_preserves_existing_rows() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        {
            let conn = store.conn.lock();
            conn.execute_batch(
                "DROP TRIGGER delegation_origin_session_deleted;
                 DROP TRIGGER delegation_origin_owner_changed;
                 DROP TABLE session_delegations;
                CREATE TABLE session_delegations (
                    agent_alias TEXT NOT NULL,
                    request_id TEXT NOT NULL,
                    request_digest TEXT NOT NULL,
                    dispatch_id TEXT,
                    route_digest TEXT,
                    notified_outcome TEXT,
                    PRIMARY KEY(agent_alias, request_id)
                );
                INSERT INTO session_delegations
                    (agent_alias, request_id, request_digest, dispatch_id, route_digest, notified_outcome)
                VALUES ('butler', 'req-legacy', 'd', 'disp-legacy', 'route-1', NULL);",
            )
            .unwrap();
        }
        drop(store);

        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        let rows = reopened.list_dispatch_bindings_page(0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].terminal_digest, None);
        assert_eq!(rows[0].proxy_continuations, 0);
        assert_eq!(rows[0].last_proxy_revision, None);
        assert_eq!(
            rows[0].proxy_continuation_state,
            ProxyContinuationState::Idle
        );

        drop(reopened);
        let reopened_again = SqliteSessionBackend::new(tmp.path()).unwrap();
        let rows = reopened_again.list_dispatch_bindings_page(0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].terminal_digest, None);
        assert_eq!(rows[0].proxy_continuations, 0);
        assert_eq!(rows[0].last_proxy_revision, None);
        assert_eq!(
            rows[0].proxy_continuation_state,
            ProxyContinuationState::Idle
        );
    }

    fn proxy_binding(
        store: &SqliteSessionBackend,
        alias: &str,
        request: &str,
        route: &str,
        dispatch: &str,
    ) -> DispatchBindingRow {
        use reiclaw_api::delegation_request::DelegationOriginScope;
        let session_key = format!("source:{alias}:{request}");
        store.set_session_agent_alias(&session_key, alias).unwrap();
        let origin = DelegationOrigin {
            session_key,
            scope: DelegationOriginScope::SessionFacts {
                operator_subject: "ab".repeat(32),
                agent_id: format!("identity:{alias}"),
                memory_session_id: None,
            },
        };
        store
            .claim_delegation_request_with_origin(alias, request, "digest", route, Some(&origin))
            .unwrap();
        store
            .bind_delegation_request(alias, request, "digest", route, dispatch)
            .unwrap();
        store
            .list_dispatch_bindings_page(0, 100)
            .unwrap()
            .into_iter()
            .find(|row| row.agent_alias == alias && row.request_id == request)
            .unwrap()
    }

    #[test]
    fn terminal_digest_receipt_survives_reopen_and_rejects_scope_changes() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let mut binding = proxy_binding(&store, "butler", "req", "route", "dispatch");
        store.record_terminal_digest(&binding, Some(10)).unwrap();
        drop(store);
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            store.list_dispatch_bindings_page(0, 10).unwrap()[0]
                .terminal_digest
                .as_deref(),
            Some("empty:10")
        );
        binding.agent_alias = "foreign".into();
        assert!(store.record_terminal_digest(&binding, None).is_err());
        binding.agent_alias = "butler".into();
        binding.route_digest = Some("other".into());
        assert!(store.record_terminal_digest(&binding, None).is_err());
        binding.route_digest = Some("route".into());
        binding.dispatch_id = "other".into();
        assert!(store.record_terminal_digest(&binding, None).is_err());
        binding.dispatch_id = "dispatch".into();
        store.record_terminal_digest(&binding, None).unwrap();
        store.record_terminal_digest(&binding, Some(10)).unwrap();
        assert_eq!(
            store.list_dispatch_bindings_page(0, 10).unwrap()[0]
                .terminal_digest
                .as_deref(),
            Some("stored")
        );
        let route = "a".repeat(64);
        store.conn.lock().execute("INSERT INTO session_delegations (agent_alias, request_id, request_digest, dispatch_id) VALUES (?1, 'old', 'digest', 'old-dispatch')", [format!("butler:{route}")]).unwrap();
        let legacy = store
            .list_dispatch_bindings_page(0, 10)
            .unwrap()
            .into_iter()
            .find(|row| row.request_id == "old")
            .unwrap();
        store.record_terminal_digest(&legacy, None).unwrap();
        assert_eq!(
            store.list_dispatch_bindings_page(0, 10).unwrap()[1]
                .terminal_digest
                .as_deref(),
            Some("stored")
        );
    }

    fn reserve_proxy(
        store: &SqliteSessionBackend,
        binding: &DispatchBindingRow,
        revision: u64,
    ) -> ProxyContinuationReservation {
        match store
            .reserve_proxy_continuation(binding, revision, 3)
            .unwrap()
        {
            ProxyContinuationAdmission::Reserved(reservation) => reservation,
            other => panic!("expected a proxy continuation reservation, got {other:?}"),
        }
    }

    #[test]
    fn proxy_continuation_migration_preserves_ambiguous_history_as_unknown() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        store
            .conn
            .lock()
            .execute_batch(
                "DROP TRIGGER delegation_origin_session_deleted;
                 DROP TRIGGER delegation_origin_owner_changed;
                 DROP TABLE session_delegations;
             CREATE TABLE session_delegations (
                agent_alias TEXT NOT NULL, request_id TEXT NOT NULL,
                request_digest TEXT NOT NULL, dispatch_id TEXT, route_digest TEXT,
                notified_outcome TEXT, proxy_continuations INTEGER NOT NULL DEFAULT 0,
                last_proxy_revision INTEGER, PRIMARY KEY(agent_alias, request_id)
             );
             INSERT INTO session_delegations VALUES
                ('old', 'r', 'd', 'dispatch', 'route', NULL, 2, 17);",
            )
            .unwrap();
        drop(store);
        for _ in 0..2 {
            let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
            let row = reopened
                .list_dispatch_bindings_page(0, 10)
                .unwrap()
                .remove(0);
            assert_eq!(row.proxy_continuations, 2);
            assert_eq!(row.last_proxy_revision, Some(17));
            assert_eq!(
                row.proxy_continuation_state,
                ProxyContinuationState::Unknown
            );
            assert_eq!(
                reopened.reserve_proxy_continuation(&row, 99, 3).unwrap(),
                ProxyContinuationAdmission::Unresolved
            );
            let alias = proxy_binding(&reopened, "new", "another-request", "route", "dispatch");
            assert_eq!(
                reopened.reserve_proxy_continuation(&alias, 100, 3).unwrap(),
                ProxyContinuationAdmission::Unresolved
            );
        }
    }

    #[test]
    fn proxy_continuation_pending_and_unknown_survive_reopen_and_new_request() {
        let tmp = TempDir::new().unwrap();
        for unknown in [false, true] {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            let dispatch = format!("dispatch-{unknown}");
            let request = format!("request-{unknown}");
            let row = proxy_binding(&store, "a", &request, "route", &dispatch);
            let reservation = reserve_proxy(&store, &row, 5);
            assert_eq!(reservation.round(), 1);
            if unknown {
                assert!(store.mark_proxy_continuation_unknown(&reservation).unwrap());
                assert!(
                    !store
                        .release_unsent_proxy_continuation(&reservation)
                        .unwrap()
                );
            }
            drop(store);
            let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
            let alias = proxy_binding(&reopened, "b", &request, "route", &dispatch);
            for binding in [&row, &alias] {
                for revision in [5, 99] {
                    assert_eq!(
                        reopened
                            .reserve_proxy_continuation(binding, revision, 3)
                            .unwrap(),
                        ProxyContinuationAdmission::Unresolved
                    );
                }
            }
            let updated = reopened
                .list_dispatch_bindings_page(0, 100)
                .unwrap()
                .into_iter()
                .find(|item| item.rowid == row.rowid)
                .unwrap();
            assert_eq!(updated.proxy_continuations, 1);
            assert_eq!(
                updated.proxy_continuation_state,
                if unknown {
                    ProxyContinuationState::Unknown
                } else {
                    ProxyContinuationState::Reserved
                }
            );
        }
    }

    #[test]
    fn proxy_continuation_same_revision_is_serialized_across_handles() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = proxy_binding(&first, "a", "r", "route", "dispatch");
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|store| {
                let barrier = Arc::clone(&barrier);
                let row = row.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.reserve_proxy_continuation(&row, 42, 3).unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, ProxyContinuationAdmission::Reserved(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| **result == ProxyContinuationAdmission::Unresolved)
                .count(),
            1
        );
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = reopened
            .list_dispatch_bindings_page(0, 10)
            .unwrap()
            .remove(0);
        assert_eq!(row.proxy_continuations, 1);
        assert_eq!(row.last_proxy_revision, Some(42));
    }

    #[test]
    fn proxy_continuation_cap_uses_durable_aggregate_for_stale_concurrent_aliases() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        let a = proxy_binding(&first, "a", "r1", "route", "dispatch");
        let b = proxy_binding(&first, "b", "r2", "route", "dispatch");
        for revision in [1, 2] {
            let reservation = reserve_proxy(&first, &a, revision);
            assert!(first.settle_proxy_continuation(&reservation).unwrap());
        }
        // Both caller snapshots still claim a zero count. A successful receipt
        // can settle before the competing transaction runs; the durable cap
        // must still prevent a fourth attempt, not merely rely on pending.
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = [(first, a.clone(), 3), (second, b.clone(), 4)]
            .into_iter()
            .map(|(store, row, revision)| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let admission = store.reserve_proxy_continuation(&row, revision, 3).unwrap();
                    if let ProxyContinuationAdmission::Reserved(reservation) = &admission {
                        assert!(store.settle_proxy_continuation(reservation).unwrap());
                    }
                    admission
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, ProxyContinuationAdmission::Reserved(_)))
                .count(),
            1
        );
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        for row in [&a, &b] {
            assert_eq!(
                reopened.reserve_proxy_continuation(row, 100, 3).unwrap(),
                ProxyContinuationAdmission::CapExceeded { prior: 3, max: 3 }
            );
        }
        assert_eq!(
            reopened
                .list_dispatch_bindings_page(0, 10)
                .unwrap()
                .iter()
                .map(|row| u64::from(row.proxy_continuations))
                .sum::<u64>(),
            3
        );
    }

    #[test]
    fn proxy_continuation_legacy_scope_and_new_alias_share_owner_but_routes_do_not() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let route = "a".repeat(64);
        store.conn.lock().execute(
            "INSERT INTO session_delegations (agent_alias, request_id, request_digest, dispatch_id)
             VALUES (?1, 'legacy', 'd', 'dispatch')", [format!("old:{route}")],
        ).unwrap();
        let a = proxy_binding(&store, "agent_%", "r1", &route, "dispatch");
        let b = proxy_binding(&store, "agent_AX", "r2", &route, "dispatch");
        let other_route = proxy_binding(&store, "agent_%", "other", "different-route", "dispatch");
        let reservation = reserve_proxy(&store, &a, 8);
        assert_eq!(
            store.reserve_proxy_continuation(&b, 9, 3).unwrap(),
            ProxyContinuationAdmission::Unresolved
        );
        let independent = reserve_proxy(&store, &other_route, 1);
        assert_eq!(independent.round(), 1);
        assert!(store.settle_proxy_continuation(&reservation).unwrap());
        assert_eq!(
            store.reserve_proxy_continuation(&b, 8, 3).unwrap(),
            ProxyContinuationAdmission::AlreadyRecorded { last_revision: 8 }
        );
        assert_eq!(
            store.reserve_proxy_continuation(&b, 7, 3).unwrap(),
            ProxyContinuationAdmission::StaleRevision { last_revision: 8 }
        );
        let next = reserve_proxy(&store, &b, 9);
        assert_eq!(next.round(), 2);
        let rows = store.list_dispatch_bindings_page(0, 10).unwrap();
        let legacy = rows.iter().find(|row| row.request_id == "legacy").unwrap();
        assert_eq!(legacy.proxy_continuations, 2);
        assert_eq!(
            legacy.proxy_continuation_state,
            ProxyContinuationState::Reserved
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.rowid == b.rowid)
                .unwrap()
                .proxy_continuations,
            0
        );
    }

    #[test]
    fn proxy_continuation_proven_unsent_retry_has_new_nonce_and_keeps_high_water() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = proxy_binding(&store, "a", "r", "route", "dispatch");
        let first = reserve_proxy(&store, &row, 10);
        assert!(store.release_unsent_proxy_continuation(&first).unwrap());
        assert!(store.release_unsent_proxy_continuation(&first).unwrap());
        drop(store);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        let stored = reopened
            .list_dispatch_bindings_page(0, 10)
            .unwrap()
            .remove(0);
        assert_eq!(stored.proxy_continuations, 0);
        assert_eq!(stored.last_proxy_revision, Some(10));
        assert_eq!(
            stored.proxy_continuation_state,
            ProxyContinuationState::Unsent
        );
        assert_eq!(
            reopened.reserve_proxy_continuation(&row, 9, 3).unwrap(),
            ProxyContinuationAdmission::StaleRevision { last_revision: 10 }
        );
        let retry = reserve_proxy(&reopened, &row, 10);
        assert_ne!(first.nonce, retry.nonce);
        assert_eq!(retry.round(), 1);
        assert!(!reopened.release_unsent_proxy_continuation(&first).unwrap());
        assert!(!reopened.settle_proxy_continuation(&first).unwrap());
        assert!(!reopened.mark_proxy_continuation_unknown(&first).unwrap());
        assert!(reopened.settle_proxy_continuation(&retry).unwrap());
        assert!(reopened.settle_proxy_continuation(&retry).unwrap());
        assert!(!reopened.release_unsent_proxy_continuation(&retry).unwrap());
        assert_eq!(
            reopened.reserve_proxy_continuation(&row, 10, 3).unwrap(),
            ProxyContinuationAdmission::AlreadyRecorded { last_revision: 10 }
        );
    }

    #[test]
    fn proxy_continuation_reservation_write_failure_does_not_consume_budget() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = proxy_binding(&store, "a", "r", "route", "dispatch");
        store.conn.lock().execute_batch(
            "CREATE TRIGGER refuse_proxy BEFORE UPDATE OF proxy_continuation_state ON session_delegations
             BEGIN SELECT RAISE(ABORT, 'reservation persistence failed'); END;",
        ).unwrap();
        assert!(store.reserve_proxy_continuation(&row, 1, 3).is_err());
        let stored = store.list_dispatch_bindings_page(0, 10).unwrap().remove(0);
        assert_eq!(stored.proxy_continuations, 0);
        assert_eq!(stored.last_proxy_revision, None);
        assert_eq!(
            stored.proxy_continuation_state,
            ProxyContinuationState::Idle
        );
    }

    #[test]
    fn proxy_continuation_settlement_failure_retains_reservation_after_restart() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = proxy_binding(&store, "a", "r", "route", "dispatch");
        let reservation = reserve_proxy(&store, &row, 1);
        store.conn.lock().execute_batch(
            "CREATE TRIGGER refuse_settlement BEFORE UPDATE OF proxy_continuation_state ON session_delegations
             WHEN NEW.proxy_continuation_state LIKE 'settled:%'
             BEGIN SELECT RAISE(ABORT, 'settlement persistence failed'); END;",
        ).unwrap();
        assert!(store.settle_proxy_continuation(&reservation).is_err());
        drop(store);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened.reserve_proxy_continuation(&row, 2, 3).unwrap(),
            ProxyContinuationAdmission::Unresolved
        );
        let stored = reopened
            .list_dispatch_bindings_page(0, 10)
            .unwrap()
            .remove(0);
        assert_eq!(stored.proxy_continuations, 1);
        assert_eq!(
            stored.proxy_continuation_state,
            ProxyContinuationState::Reserved
        );
    }

    #[test]
    fn proxy_continuation_rejects_changed_binding_and_invalid_stored_state() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = proxy_binding(&store, "a", "r", "route", "dispatch");
        let mut wrong = row.clone();
        wrong.agent_alias = "b".to_string();
        assert!(store.reserve_proxy_continuation(&wrong, 1, 3).is_err());
        wrong = row.clone();
        wrong.request_id = "other".to_string();
        assert!(store.reserve_proxy_continuation(&wrong, 1, 3).is_err());
        wrong = row.clone();
        wrong.route_digest = Some("other-route".to_string());
        assert!(store.reserve_proxy_continuation(&wrong, 1, 3).is_err());
        assert!(store.reserve_proxy_continuation(&row, 1, 4).is_err());
        assert!(store.reserve_proxy_continuation(&row, u64::MAX, 3).is_err());
        for invalid in ["invalid", "reserved:not-a-uuid", "unknown:legacy"] {
            store
                .conn
                .lock()
                .execute(
                    "UPDATE session_delegations SET proxy_continuation_state = ?1",
                    [invalid],
                )
                .unwrap();
            assert!(store.reserve_proxy_continuation(&row, 1, 3).is_err());
            assert!(store.list_dispatch_bindings_page(0, 10).is_err());
        }
    }

    #[test]
    fn proxy_continuation_unscoped_legacy_dispatch_requires_reconciliation() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let row = proxy_binding(&store, "new", "r", "route", "dispatch");
        store.conn.lock().execute(
            "INSERT INTO session_delegations (agent_alias, request_id, request_digest, dispatch_id)
             VALUES ('old', 'legacy', 'd', 'dispatch')", [],
        ).unwrap();
        assert!(store.reserve_proxy_continuation(&row, 1, 3).is_err());
        assert_eq!(
            store.list_dispatch_bindings_page(0, 10).unwrap()[0].proxy_continuations,
            0
        );
    }
}
