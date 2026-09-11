//! Reading history back out.
//!
//! Two reconstruction paths live here, deliberately:
//!
//! * [`incl_at`] is the production path — a checkpoint plus the deltas since
//!   it, so cost is bounded by recent churn rather than by total history.
//! * [`incl_at_naive`] replays last-value-per-path over all of history. It is
//!   O(subtree x history) and must never run on a hot path, but it is
//!   obviously correct, which makes it the oracle the fast path is tested
//!   against.
//!
//! If those two ever disagree, the fast path is wrong.

use super::Store;
use crate::model::{PathId, RootId, ScanId};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};

/// Subtree membership as of scan `:s` — strictly what was alive at that moment.
const SUB_ALIVE_AT: &str = "
  WITH RECURSIVE sub(path_id) AS (
    SELECT :p
    UNION ALL
    SELECT c.path_id FROM path c JOIN sub ON c.parent_id = sub.path_id
    WHERE c.born_scan <= :s AND (c.died_scan IS NULL OR :s < c.died_scan)
  )";

/// Resolve a wall-clock instant to the most recent scan at or before it.
///
/// Callers should surface the resolved scan to the user rather than echoing
/// back the requested time — "nearest scan: 14:35:02" is honest, silently
/// pretending we have a sample for 14:37:19 is not.
pub fn resolve_scan(store: &Store, root_id: RootId, at: i64) -> Result<Option<(ScanId, i64)>> {
    Ok(store
        .conn
        .query_row(
            "SELECT scan_id, started_at FROM scan
             WHERE root_id = ?1 AND started_at <= ?2 AND status = 'ok'
             ORDER BY started_at DESC LIMIT 1",
            params![root_id, at],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Inclusive (subtree) size of `path_id` as of `scan_id`, via checkpoint+delta.
///
/// Returns `(bytes, blocks)`.
pub fn incl_at(store: &Store, path_id: PathId, scan_id: ScanId) -> Result<(i64, i64)> {
    let cp: Option<(ScanId, i64, i64)> = store
        .conn
        .query_row(
            "SELECT scan_id, incl_bytes, incl_blocks FROM checkpoint
             WHERE path_id = ?1 AND scan_id <= ?2 ORDER BY scan_id DESC LIMIT 1",
            params![path_id, scan_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let Some((cp_scan, cp_bytes, cp_blocks)) = cp else {
        // No keyframe yet (only possible before the first scan of this entity).
        return incl_at_naive(store, path_id, scan_id);
    };

    // Membership for the *delta* term is wider than "alive at :s": a path that
    // was deleted between the checkpoint and :s emitted a negative event that
    // must still be counted, even though it is not part of the tree at :s.
    // Filtering it out here would silently overstate every size after any
    // deletion.
    let sql = "
      WITH RECURSIVE sub(path_id) AS (
        SELECT :p
        UNION ALL
        SELECT c.path_id FROM path c JOIN sub ON c.parent_id = sub.path_id
        WHERE c.born_scan <= :s AND (c.died_scan IS NULL OR c.died_scan > :cp)
      )
      SELECT COALESCE(SUM(e.d_bytes), 0), COALESCE(SUM(e.d_blocks), 0)
      FROM size_event e JOIN sub USING(path_id)
      WHERE e.scan_id > :cp AND e.scan_id <= :s";

    let (db, dk): (i64, i64) = store.conn.query_row(
        sql,
        rusqlite::named_params! { ":p": path_id, ":s": scan_id, ":cp": cp_scan },
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    Ok((cp_bytes + db, cp_blocks + dk))
}

/// Obviously-correct but slow reconstruction: last value per path, summed.
///
/// The oracle for [`incl_at`]. Never call this on a hot path.
pub fn incl_at_naive(store: &Store, path_id: PathId, scan_id: ScanId) -> Result<(i64, i64)> {
    let sql = format!(
        "{SUB_ALIVE_AT}
         SELECT COALESCE(SUM(own_bytes), 0), COALESCE(SUM(own_blocks), 0) FROM (
           SELECT e.own_bytes, e.own_blocks,
                  ROW_NUMBER() OVER (PARTITION BY e.path_id ORDER BY e.scan_id DESC) rn
           FROM size_event e JOIN sub USING(path_id)
           WHERE e.scan_id <= :s
         ) WHERE rn = 1"
    );
    Ok(store.conn.query_row(
        &sql,
        rusqlite::named_params! { ":p": path_id, ":s": scan_id },
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Sum of `d_bytes` over a subtree across `(s1, s2]`.
///
/// This is the left-hand side of the core invariant asserted in
/// `tests/store_invariants.rs`:
///
/// ```text
/// SUM(d_bytes) over subtree(A) in (S1, S2]  ==  incl(A, S2) - incl(A, S1)
/// ```
pub fn subtree_delta(store: &Store, path_id: PathId, s1: ScanId, s2: ScanId) -> Result<(i64, i64)> {
    let sql = "
      WITH RECURSIVE sub(path_id) AS (
        SELECT :p
        UNION ALL
        SELECT c.path_id FROM path c JOIN sub ON c.parent_id = sub.path_id
        WHERE c.born_scan <= :s2 AND (c.died_scan IS NULL OR c.died_scan > :s1)
      )
      SELECT COALESCE(SUM(e.d_bytes), 0), COALESCE(SUM(e.d_blocks), 0)
      FROM size_event e JOIN sub USING(path_id)
      WHERE e.scan_id > :s1 AND e.scan_id <= :s2";
    Ok(store.conn.query_row(
        sql,
        rusqlite::named_params! { ":p": path_id, ":s1": s1, ":s2": s2 },
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Which end of the distribution to report.
///
/// Not a post-hoc reversal: the biggest shrinkers are at the opposite end of
/// the sort, so reversing a top-N of gainers returns the *smallest* gainers and
/// never finds a loser at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extreme {
    Gainers,
    Losers,
}

impl Extreme {
    fn order(self) -> &'static str {
        match self {
            Extreme::Gainers => "DESC",
            Extreme::Losers => "ASC",
        }
    }

    /// Restrict to the direction actually asked for.
    ///
    /// Ordering alone is not enough: ask for the 20 biggest shrinkers on a
    /// tree with only three, and ascending order happily fills the rest with
    /// the *smallest growers*, printing "+120 B" under a heading that says
    /// "what shrank".
    fn having(self) -> &'static str {
        match self {
            Extreme::Gainers => "> 0",
            Extreme::Losers => "< 0",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gainer {
    pub path_id: PathId,
    pub parent_id: Option<PathId>,
    pub delta_bytes: i64,
    pub delta_blocks: i64,
}

/// "Which single directory's *own* files grew?"
///
/// Points straight at the culprit rather than listing it alongside all nine of
/// its ancestors. A pure indexed range scan over `size_event_by_scan`.
pub fn gainers_exclusive(
    store: &Store,
    root_id: RootId,
    s1: ScanId,
    s2: ScanId,
    limit: i64,
    which: Extreme,
) -> Result<Vec<Gainer>> {
    let sql = format!(
        "SELECT e.path_id, p.parent_id, SUM(e.d_bytes) AS db, SUM(e.d_blocks) AS dk
         FROM size_event e
         JOIN path p ON p.path_id = e.path_id
         WHERE e.scan_id > ?2 AND e.scan_id <= ?3 AND p.root_id = ?1
         GROUP BY e.path_id
         HAVING db {}
         ORDER BY db {}
         LIMIT ?4",
        which.having(),
        which.order()
    );
    let mut st = store.conn.prepare(&sql)?;
    let rows = st.query_map(params![root_id, s1, s2, limit], |r| {
        Ok(Gainer {
            path_id: r.get(0)?,
            parent_id: r.get(1)?,
            delta_bytes: r.get(2)?,
            delta_blocks: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// "Which subtree grew?" — the ancestor-chain rollup.
///
/// This is the query no prior art has, and it is genuinely cheap: cost is
/// `O(churn x mean_depth)`, which for a 24-hour window on this machine is
/// ~358 x 9.26 ~= 3,300 additions.
pub fn gainers_inclusive(
    store: &Store,
    root_id: RootId,
    s1: ScanId,
    s2: ScanId,
    limit: i64,
    which: Extreme,
) -> Result<Vec<Gainer>> {
    let sql = format!(
        "WITH RECURSIVE
           churn AS (
             SELECT e.path_id, SUM(e.d_bytes) AS db, SUM(e.d_blocks) AS dk
             FROM size_event e
             JOIN path p ON p.path_id = e.path_id
             WHERE e.scan_id > ?2 AND e.scan_id <= ?3 AND p.root_id = ?1
             GROUP BY e.path_id
             HAVING db <> 0 OR dk <> 0
           ),
           anc(anc_id, cur_id, db, dk) AS (
             SELECT path_id, path_id, db, dk FROM churn
             UNION ALL
             SELECT p.parent_id, p.parent_id, a.db, a.dk
             FROM anc a JOIN path p ON p.path_id = a.cur_id
             WHERE p.parent_id IS NOT NULL
           )
         SELECT a.anc_id, pp.parent_id, SUM(a.db) AS tb, SUM(a.dk) AS tk
         FROM anc a JOIN path pp ON pp.path_id = a.anc_id
         GROUP BY a.anc_id
         HAVING tb {}
         ORDER BY tb {}
         LIMIT ?4",
        which.having(),
        which.order()
    );
    let mut st = store.conn.prepare(&sql)?;
    let rows = st.query_map(params![root_id, s1, s2, limit], |r| {
        Ok(Gainer {
            path_id: r.get(0)?,
            parent_id: r.get(1)?,
            delta_bytes: r.get(2)?,
            delta_blocks: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Hide ancestors whose growth is fully explained by one child.
///
/// Inclusive gainers naturally return every node on the path to a change:
/// `/home`, `/home/steven`, `/home/steven/dutime-demo`, `.../nested`,
/// `.../nested/deep` — nine rows all reporting the same 2.9 GiB, pushing the
/// answer off the bottom of the screen. When a single child accounts for
/// essentially all of a directory's growth, that directory is just a signpost;
/// the useful row is the deepest one that still explains the number.
///
/// A directory is kept when its growth is genuinely its own or is spread across
/// several children — which is exactly the case where the parent *is* the
/// insight ("your growth is the whole of ~/.cache, not any one thing in it").
///
/// `ratio` is the share a single child must exceed for its parent to be
/// suppressed. 0.9 is a good default: high enough that a directory with two
/// real contributors survives, low enough to collapse a deep chain.
pub fn collapse_ancestors(gainers: &[Gainer], ratio: f64) -> Vec<Gainer> {
    use std::collections::HashMap;

    // Largest single-child delta seen for each parent.
    let mut best_child: HashMap<PathId, i64> = HashMap::new();
    for g in gainers {
        if let Some(p) = g.parent_id {
            let e = best_child.entry(p).or_insert(0);
            if g.delta_bytes.abs() > e.abs() {
                *e = g.delta_bytes;
            }
        }
    }

    gainers
        .iter()
        .filter(|g| match best_child.get(&g.path_id) {
            // Keep a node whose own delta is not essentially one child's.
            Some(&child) => {
                g.delta_bytes == 0
                    || (child as f64 / g.delta_bytes as f64) < ratio
                    || child.signum() != g.delta_bytes.signum()
            }
            None => true,
        })
        .cloned()
        .collect()
}

/// Reconstruct a path's full path from the dictionary, as raw OS bytes.
pub fn full_path(store: &Store, path_id: PathId) -> Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut cur = Some(path_id);
    while let Some(id) = cur {
        let row: Option<(Vec<u8>, Option<PathId>)> = store
            .conn
            .query_row("SELECT name, parent_id FROM path WHERE path_id = ?1", params![id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((name, parent)) = row else { break };
        parts.push(name);
        cur = parent;
    }
    parts.reverse();
    let mut p = std::path::PathBuf::new();
    for (i, seg) in parts.into_iter().enumerate() {
        if i == 0 {
            p.push(std::path::PathBuf::from(OsString::from_vec(seg)));
        } else {
            p.push(OsString::from_vec(seg));
        }
    }
    Ok(p)
}
