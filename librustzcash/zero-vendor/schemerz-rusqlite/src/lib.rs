//! An adapter enabling use of the schemerz schema migration library with
//! SQLite3.
//!
//! # Examples:
//!
//! ```rust
//! extern crate rusqlite;
//! #[macro_use]
//! extern crate schemerz;
//! extern crate schemerz_rusqlite;
//! extern crate uuid;
//!
//! use std::collections::HashSet;
//!
//! use rusqlite::{params, Connection, Transaction, Error as RusqliteError};
//! use schemerz::{Migration, Migrator};
//! use schemerz_rusqlite::{RusqliteAdapter, RusqliteAdapterError, RusqliteMigration};
//! use uuid::uuid;
//!
//! struct MyExampleMigration;
//! migration!(
//!     MyExampleMigration,
//!     uuid!("4885e8ab-dafa-4d76-a565-2dee8b04ef60"),
//!     [],
//!     "An example migration without dependencies.");
//!
//! impl RusqliteMigration for MyExampleMigration {
//!     type Error = RusqliteError;
//!
//!     fn up(&self, transaction: &Transaction) -> Result<(), RusqliteAdapterError> {
//!         transaction.execute("CREATE TABLE my_example (id integer PRIMARY KEY);", params![])?;
//!         Ok(())
//!     }
//!
//!     fn down(&self, transaction: &Transaction) -> Result<(), RusqliteAdapterError> {
//!         transaction.execute("DROP TABLE my_example;", params![])?;
//!         Ok(())
//!     }
//! }
//!
//! fn main() {
//!     let mut conn = Connection::open_in_memory().unwrap();
//!     let adapter = RusqliteAdapter::new(&mut conn, None);
//!
//!     let mut migrator = Migrator::new(adapter);
//!
//!     let migration = Box::new(MyExampleMigration {});
//!     migrator.register(migration);
//!     migrator.up(None);
//! }
//! ```
#![warn(clippy::all)]
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::error::Error;
use std::marker::{PhantomData, Send, Sync};

use rusqlite::{params, Connection, Error as RusqliteError, Transaction};
use uuid::Uuid;

use schemerz::{Adapter, Migration};

/// SQlite-specific trait for schema migrations.
pub trait RusqliteMigration: Migration<Uuid> {
    type Error: From<RusqliteError>;

    /// Apply a migration to the database using a transaction.
    fn up(&self, _transaction: &Transaction<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Revert a migration to the database using a transaction.
    fn down(&self, _transaction: &Transaction<'_>) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub type RusqliteAdapterError = RusqliteError;

struct WrappedUuid(Uuid);

impl rusqlite::types::FromSql for WrappedUuid {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(WrappedUuid(Uuid::from_slice(value.as_blob()?).map_err(
            |e| rusqlite::types::FromSqlError::Other(Box::new(e)),
        )?))
    }
}

/// Adapter between schemerz and SQLite.
pub struct RusqliteAdapter<'a, E> {
    conn: &'a mut Connection,
    migration_metadata_table: String,
    _err: PhantomData<E>,
}

impl<'a, E> RusqliteAdapter<'a, E> {
    /// Construct a SQLite schemerz adapter.
    ///
    /// `table_name` specifies the name of the table that schemerz will use
    /// for storing metadata about applied migrations. If `None`, a default
    /// will be used.
    ///
    /// ```rust
    /// # extern crate rusqlite;
    /// # use rusqlite::{Error as RusqliteError};
    /// #
    /// # fn main() {
    /// let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    /// let adapter: schemerz_rusqlite::RusqliteAdapter<RusqliteError> = schemerz_rusqlite::RusqliteAdapter::new(&mut conn, None);
    /// # }
    /// ```
    pub fn new(conn: &'a mut Connection, table_name: Option<String>) -> RusqliteAdapter<'a, E> {
        RusqliteAdapter {
            conn,
            migration_metadata_table: table_name.unwrap_or_else(|| "_schemerz".into()),
            _err: PhantomData,
        }
    }

    /// Initialize the schemerz metadata schema. This must be called before
    /// using `Migrator` with this adapter. This is safe to call multiple times.
    pub fn init(&self) -> Result<(), RusqliteError> {
        self.conn.execute(
            &format!(
                r#"
                    CREATE TABLE IF NOT EXISTS {} (
                        id blob PRIMARY KEY
                    )
                "#,
                self.migration_metadata_table
            ),
            params![],
        )?;
        Ok(())
    }
}

impl<'a, E> Adapter<Uuid> for RusqliteAdapter<'a, E>
where
    E: From<RusqliteError> + Sync + Send + Error + 'static,
{
    type MigrationType = Box<dyn RusqliteMigration<Error = E>>;

    type Error = E;

    fn applied_migrations(&mut self) -> Result<HashSet<Uuid>, Self::Error> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id FROM {};",
            self.migration_metadata_table
        ))?;
        // TODO: have to do this rather than `collect` because Rusqlite has an
        // interface that goes against map conventions.
        let rows = stmt.query_map(params![], |row| row.get::<_, WrappedUuid>(0))?;
        let mut ids = HashSet::new();
        for row in rows {
            ids.insert(row?.0);
        }
        Ok(ids)
    }

    fn apply_migration(&mut self, migration: &Self::MigrationType) -> Result<(), Self::Error> {
        let trans = self.conn.transaction()?;
        migration.up(&trans)?;
        let uuid = migration.id();
        let uuid_bytes = &uuid.as_bytes()[..];
        trans.execute(
            &format!(
                "INSERT INTO {} (id) VALUES (?1);",
                self.migration_metadata_table
            ),
            [&uuid_bytes],
        )?;
        trans.commit().map_err(|e| e.into())
    }

    fn revert_migration(&mut self, migration: &Self::MigrationType) -> Result<(), Self::Error> {
        let trans = self.conn.transaction()?;
        migration.down(&trans)?;
        let uuid = migration.id();
        let uuid_bytes = &uuid.as_bytes()[..];
        trans.execute(
            &format!(
                "DELETE FROM {} WHERE id = ?1;",
                self.migration_metadata_table
            ),
            [&uuid_bytes],
        )?;
        trans.commit().map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Error as RusqliteError;
    use schemerz::test_schemerz_adapter;
    use schemerz::testing::*;

    impl RusqliteMigration for TestMigration<Uuid> {
        type Error = RusqliteError;
    }

    impl<'a> TestAdapter<Uuid> for RusqliteAdapter<'a, RusqliteError> {
        fn mock(id: Uuid, dependencies: HashSet<Uuid>) -> Self::MigrationType {
            Box::new(TestMigration::new(id, dependencies))
        }
    }

    fn build_test_connection() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    fn build_test_adapter(conn: &mut Connection) -> RusqliteAdapter<'_, RusqliteError> {
        let adapter = RusqliteAdapter::new(conn, None);
        adapter.init().unwrap();
        adapter
    }

    fn uuid_iter() -> impl Iterator<Item = Uuid> {
        (0..).map(|v| Uuid::from_fields(v as u32, v, v, &[0; 8]))
    }

    // [zero] @claude The trailing comma after the third argument is load-bearing in the
    // wrong direction: with it, this call falls through to the variadic arm of
    // `test_schemerz_adapter!` with an empty test-name list and expands to nothing, so
    // the adapter suite silently runs zero tests. Without it the three-argument arm
    // matches and generates all five. Present in the published 0.370.0.
    test_schemerz_adapter!(
        let mut conn = build_test_connection(),
        build_test_adapter(&mut conn),
        uuid_iter()
    );
}
