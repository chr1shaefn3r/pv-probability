//! Reading one entity's rows in time slices, each on its own connection, in parallel.
//!
//! SQLite answers a query on one thread, and for a long history most of a run is spent
//! stepping through it: every row is found through the `(metadata_id, start_ts)` index and
//! then fetched from the table. Cut the time range into slices, read each on a connection
//! of its own, and put the results back together in order, and the read scales with the
//! cores - while returning exactly the rows the single query would have.
//!
//! That only holds with SQLite built without its process-wide locks: by default the
//! bundled library shares one page cache, behind one mutex, between every connection,
//! and counts every allocation behind another. `.cargo/config.toml` turns both off.

use std::path::PathBuf;

use anyhow::Result;
use rayon::prelude::*;
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::source::open_database;
use crate::source::schema::value_to_timestamp;

/// Rows below this are read in one query: opening another connection costs about as much
/// as reading three hundred rows, so a slice has to be well past that to pay for itself.
pub const MIN_ROWS_PER_SLICE: i64 = 2_048;

/// Which of an entity's rows one query reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slice {
    /// The rows whose time is NULL. A query ordered by time returns them first, so an
    /// unbounded read cut into slices reads them first too.
    Untimed,
    /// The rows from `from` (inclusive) to `to` (exclusive). Open at both ends it is every
    /// row, the untimed ones included, as a query without a condition reads them.
    Span { from: Option<i64>, to: Option<i64> },
}

impl Slice {
    /// Everything between two optional bounds, as a single query would read it.
    pub fn window(from: Option<i64>, to: Option<i64>) -> Self {
        Self::Span { from, to }
    }

    /// The SQL condition on `column` for this slice, starting with ` AND`, with the
    /// bounds appended to `params` as `?N` placeholders.
    pub fn condition(&self, column: &str, params: &mut Vec<Value>) -> String {
        let (from, to) = match *self {
            Self::Untimed => return format!(" AND \"{column}\" IS NULL"),
            Self::Span { from, to } => (from, to),
        };
        let mut condition = String::new();
        if let Some(from) = from {
            params.push(Value::Integer(from));
            condition += &format!(" AND \"{column}\" >= ?{}", params.len());
        }
        if let Some(to) = to {
            params.push(Value::Integer(to));
            condition += &format!(" AND \"{column}\" < ?{}", params.len());
        }
        condition
    }
}

/// Cut `window` into at most `threads` slices of about equal time, given that the rows
/// it holds run from `first` to `last` (inclusive) and that `rows` of them are expected.
///
/// Every row in the window lands in exactly one slice, and the slices come in time order,
/// so reading them one after another returns what reading the window would. A window
/// open at both ends starts with the untimed rows, on their own: asking for them along
/// with the first span (`IS NULL OR ... < ?`) would make SQLite walk every index entry
/// the entity has rather than seek to the span.
pub fn plan(window: Slice, first: i64, last: i64, rows: i64, threads: usize) -> Vec<Slice> {
    let Slice::Span { from, to } = window else {
        return vec![window];
    };
    let count = (rows / MIN_ROWS_PER_SLICE).clamp(1, threads.max(1) as i64);
    if count <= 1 || last <= first {
        return vec![window];
    }

    let span = last - first + 1;
    let mut slices = Vec::with_capacity(count as usize + 1);
    if from.is_none() && to.is_none() {
        slices.push(Slice::Untimed);
    }
    let mut start = from;
    for index in 1..count {
        let cut = first + span * index / count;
        slices.push(Slice::Span {
            from: start,
            to: Some(cut),
        });
        start = Some(cut);
    }
    slices.push(Slice::Span { from: start, to });
    slices
}

/// The earliest and latest value of `time` among the rows matching `key = ?1` inside
/// `window`, or `None` when there are none.
///
/// Two queries rather than one: SQLite answers a lone `MIN` or `MAX` straight from the
/// index, but computes both together by scanning every matching row.
pub fn time_bounds(
    conn: &Connection,
    table: &str,
    key: &str,
    key_value: &Value,
    time: &str,
    window: Slice,
) -> Result<Option<(i64, i64)>> {
    let bound = |aggregate: &str| -> Result<Option<i64>> {
        let mut params = vec![key_value.clone()];
        let condition = window.condition(time, &mut params);
        let sql = format!(
            "SELECT {aggregate}(\"{time}\") FROM \"{table}\" WHERE \"{key}\" = ?1{condition}"
        );
        let value: Value =
            conn.query_row(&sql, rusqlite::params_from_iter(params), |row| row.get(0))?;
        Ok(value_to_timestamp(&value))
    };
    Ok(bound("MIN")?.zip(bound("MAX")?))
}

/// Read every slice with `read` and return the results in slice order.
///
/// The spans are read in parallel, each on a connection of its own, when there are
/// several. The untimed rows are a single index lookup, not worth a connection, so they
/// are read on the one given. So is everything from a database without a file behind it -
/// an in-memory one, in the tests - which cannot be opened twice.
pub fn read_all<T, F>(conn: &Connection, slices: &[Slice], read: F) -> Result<Vec<T>>
where
    T: Send,
    F: Fn(&Connection, Slice) -> Result<T> + Sync,
{
    let spans = slices.iter().filter(|slice| **slice != Slice::Untimed);
    let path = match database_path(conn) {
        Some(path) if spans.count() >= 2 => path,
        _ => return slices.iter().map(|slice| read(conn, *slice)).collect(),
    };

    let mut parts = Vec::with_capacity(slices.len());
    let mut rest = slices;
    while let Some((Slice::Untimed, tail)) = rest.split_first() {
        parts.push(read(conn, Slice::Untimed)?);
        rest = tail;
    }
    let spans: Vec<T> = rest
        .par_iter()
        .map(|slice| read(&open_database(&path)?, *slice))
        .collect::<Result<_>>()?;
    parts.extend(spans);
    Ok(parts)
}

/// The file a connection reads, if it reads one.
fn database_path(conn: &Connection) -> Option<PathBuf> {
    conn.path()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a row with this time belongs to `slice`.
    fn holds(slice: Slice, time: Option<i64>) -> bool {
        match (slice, time) {
            (Slice::Untimed, time) => time.is_none(),
            (
                Slice::Span {
                    from: None,
                    to: None,
                },
                _,
            ) => true,
            (Slice::Span { .. }, None) => false,
            (Slice::Span { from, to }, Some(time)) => {
                from.is_none_or(|from| time >= from) && to.is_none_or(|to| time < to)
            }
        }
    }

    /// Which slice each of `times` lands in, checking it lands in exactly one.
    fn assignment(slices: &[Slice], times: &[Option<i64>]) -> Vec<usize> {
        times
            .iter()
            .map(|time| {
                let owners: Vec<usize> = slices
                    .iter()
                    .enumerate()
                    .filter(|(_, slice)| holds(**slice, *time))
                    .map(|(index, _)| index)
                    .collect();
                assert_eq!(owners.len(), 1, "{time:?} is in {owners:?} of {slices:?}");
                owners[0]
            })
            .collect()
    }

    #[test]
    fn a_short_read_stays_one_query() {
        let window = Slice::window(None, None);
        assert_eq!(plan(window, 0, 3_600 * 100, 100, 8), vec![window]);
        assert_eq!(
            plan(window, 0, 0, 1_000_000, 8),
            vec![window],
            "one instant"
        );
        assert_eq!(
            plan(window, 0, 1 << 30, 1 << 20, 1),
            vec![window],
            "one thread"
        );
        assert_eq!(
            plan(Slice::Untimed, 0, 1 << 30, 1 << 20, 8),
            [Slice::Untimed]
        );
    }

    #[test]
    fn a_long_read_is_cut_into_ordered_slices_that_cover_every_row_once() {
        let mut times: Vec<Option<i64>> = vec![None, None];
        times.extend((0..17_520).map(|hour| Some(1_700_000_000 + hour * 3_600)));
        let (first, last) = (times[2].unwrap(), times.last().unwrap().unwrap());
        let slices = plan(Slice::window(None, None), first, last, 17_520, 4);

        assert_eq!(slices.len(), 5, "the untimed rows and four spans");
        assert_eq!(slices[0], Slice::Untimed, "rows without a time go first");
        assert_eq!(
            slices[1],
            Slice::Span {
                from: None,
                to: Some(first + (last - first + 1) / 4)
            },
            "the first span keeps the window's lower end"
        );
        assert!(
            matches!(slices[4], Slice::Span { to: None, .. }),
            "the last keeps its upper end"
        );

        let owners = assignment(&slices, &times);
        assert!(
            owners.windows(2).all(|pair| pair[0] <= pair[1]),
            "in the order a query sorted by time returns them"
        );
        assert_eq!(owners.iter().filter(|owner| **owner == 0).count(), 2);
        for index in 1..5 {
            let share = owners.iter().filter(|owner| **owner == index).count();
            assert!(
                (4_300..=4_460).contains(&share),
                "slice {index} has {share} rows"
            );
        }
    }

    #[test]
    fn slices_never_reach_outside_the_window() {
        let window = Slice::window(Some(1_000), Some(900_000));
        let slices = plan(window, 1_000, 899_999, 250_000, 3);
        assert_eq!(slices.len(), 3, "a bounded read has no untimed rows");
        assert!(matches!(
            slices[0],
            Slice::Span {
                from: Some(1_000),
                ..
            }
        ));
        assert!(matches!(
            slices[2],
            Slice::Span {
                to: Some(900_000),
                ..
            }
        ));
        let times = [Some(1_000), Some(450_000), Some(899_999)];
        assert_eq!(assignment(&slices, &times), [0, 1, 2]);
        assert!(slices.iter().all(|slice| !holds(*slice, None)));
        assert!(slices.iter().all(|slice| !holds(*slice, Some(900_000))));
    }

    #[test]
    fn conditions_bind_their_bounds_in_order() {
        let mut params = vec![Value::Integer(7)];
        let both = Slice::window(Some(10), Some(20));
        assert_eq!(
            both.condition("t", &mut params),
            " AND \"t\" >= ?2 AND \"t\" < ?3"
        );
        assert_eq!(
            params,
            [Value::Integer(7), Value::Integer(10), Value::Integer(20)]
        );

        let mut params = Vec::new();
        assert_eq!(
            Slice::window(None, Some(20)).condition("t", &mut params),
            " AND \"t\" < ?1"
        );
        assert_eq!(params, [Value::Integer(20)]);
        assert_eq!(
            Slice::Untimed.condition("t", &mut params),
            " AND \"t\" IS NULL"
        );
        assert_eq!(params.len(), 1, "the untimed rows bind nothing");
        assert_eq!(
            Slice::window(None, None).condition("t", &mut Vec::new()),
            ""
        );
    }
}
