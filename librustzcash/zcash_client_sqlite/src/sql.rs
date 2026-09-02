//! [zero] @claude Checked conversions between `u64`/`usize` and SQLite's integer type.
//!
//! SQLite's only integer type is a signed 64-bit one, so values above [`i64::MAX`] have no
//! representation. `rusqlite` supplied `ToSql`/`FromSql` implementations for `u64` and
//! `usize` up to 0.37 and removed them in 0.38, because the conversion is lossy in both
//! directions: a `u64` above [`i64::MAX`] wrapped to a negative integer on write, and a
//! negative column value wrapped to a huge `u64` on read.
//!
//! This module is the single place where that conversion happens, and it is checked. Use
//! [`SqlU64`] to bind a `u64` as a parameter, and the [`RowExt`] methods to read one back.

use rusqlite::{
    Row, RowIndex,
    types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef},
};

/// A `u64` that can cross the SQLite boundary.
///
/// Binding a value greater than [`i64::MAX`] fails rather than wrapping; reading a negative
/// column value fails rather than wrapping. The wallet stores no quantity that can reach
/// that bound — the largest are note commitment tree positions and zatoshi amounts, both far
/// below it — so in practice neither error can be produced by data this crate wrote. They
/// exist to make a corrupted or foreign database fail loudly instead of silently returning
/// nonsense.
///
/// The inner value is exposed directly: this is a transparent carrier for a primitive at the
/// storage boundary, not a domain type, and it must not be given domain meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SqlU64(pub(crate) u64);

impl SqlU64 {
    /// Binds a `usize` as an unsigned SQLite integer.
    ///
    /// This conversion is infallible: Rust has no target whose `usize` is wider than 64
    /// bits, so every `usize` fits. The narrowing direction is the fallible one, and it
    /// lives in [`RowExt::get_usize`].
    pub(crate) fn from_usize(value: usize) -> Self {
        SqlU64(value as u64)
    }
}

impl From<u64> for SqlU64 {
    fn from(value: u64) -> Self {
        SqlU64(value)
    }
}

impl From<SqlU64> for u64 {
    fn from(value: SqlU64) -> Self {
        value.0
    }
}

impl ToSql for SqlU64 {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        i64::try_from(self.0)
            .map(ToSqlOutput::from)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }
}

impl FromSql for SqlU64 {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let raw = i64::column_result(value)?;
        u64::try_from(raw)
            .map(SqlU64)
            .map_err(|_| FromSqlError::OutOfRange(raw))
    }
}

/// A `usize` read back from SQLite.
///
/// Narrowing is the fallible direction — a 64-bit column value need not fit a 32-bit
/// `usize`, which is reachable on wasm — so this exists only to read. Bind with
/// [`SqlU64::from_usize`], whose widening direction cannot fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SqlUsize(pub(crate) usize);

impl FromSql for SqlUsize {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let raw = i64::column_result(value)?;
        usize::try_from(raw)
            .map(SqlUsize)
            .map_err(|_| FromSqlError::OutOfRange(raw))
    }
}

/// Reads of unsigned integer columns.
///
/// `row.get::<_, u64>(i)` no longer compiles; these are its checked replacements. See the
/// module documentation for why the conversion cannot be implicit.
pub(crate) trait RowExt {
    /// Reads a non-null column as a `u64`.
    ///
    /// Fails if the stored value is negative.
    fn get_u64<I: RowIndex>(&self, idx: I) -> rusqlite::Result<u64>;

    /// Reads a nullable column as an `Option<u64>`.
    ///
    /// Fails if the stored value is present and negative.
    fn get_opt_u64<I: RowIndex>(&self, idx: I) -> rusqlite::Result<Option<u64>>;

    /// Reads a non-null column as a `usize`.
    ///
    /// Fails if the stored value is negative, or does not fit in a `usize` on this
    /// platform. The latter is reachable on a 32-bit target, including wasm.
    fn get_usize<I: RowIndex>(&self, idx: I) -> rusqlite::Result<usize>;
}

impl RowExt for Row<'_> {
    fn get_u64<I: RowIndex>(&self, idx: I) -> rusqlite::Result<u64> {
        self.get::<_, SqlU64>(idx).map(|v| v.0)
    }

    fn get_opt_u64<I: RowIndex>(&self, idx: I) -> rusqlite::Result<Option<u64>> {
        self.get::<_, Option<SqlU64>>(idx)
            .map(|v| v.map(|SqlU64(n)| n))
    }

    fn get_usize<I: RowIndex>(&self, idx: I) -> rusqlite::Result<usize> {
        self.get::<_, SqlUsize>(idx).map(|v| v.0)
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{RowExt, SqlU64};

    fn roundtrip(value: u64) -> rusqlite::Result<u64> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE t (v INTEGER NOT NULL);")?;
        conn.execute("INSERT INTO t (v) VALUES (?)", [SqlU64(value)])?;
        conn.query_row("SELECT v FROM t", [], |row| row.get_u64(0))
    }

    #[test]
    fn roundtrips_representable_values() {
        for value in [0, 1, 42, u64::from(u32::MAX), i64::MAX as u64] {
            assert_eq!(roundtrip(value).unwrap(), value);
        }
    }

    #[test]
    fn binding_above_i64_max_fails() {
        assert!(matches!(
            roundtrip(i64::MAX as u64 + 1),
            Err(rusqlite::Error::ToSqlConversionFailure(_)),
        ));
    }

    #[test]
    fn reading_a_negative_value_fails() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER NOT NULL); INSERT INTO t (v) VALUES (-1);")
            .unwrap();
        // The failure is what matters, and that it names the offending column: silently
        // wrapping -1 to 18446744073709551615 is the behaviour being guarded against.
        for read in [
            conn.query_row("SELECT v FROM t", [], |row| row.get_u64(0)),
            conn.query_row("SELECT v FROM t", [], |row| {
                row.get_usize(0).map(|v| v as u64)
            }),
        ] {
            assert!(matches!(
                read,
                Err(rusqlite::Error::IntegralValueOutOfRange(0, -1)),
            ));
        }
    }
}
