//! Every store statement binds its own values here, so the performance
//! witness counts the bytes a write binds from the values themselves
//! (`sql_bytes_written`, FIG-7581): nothing rebuilds statement text to
//! count them. A text counts its bytes, a blob its bytes, a number eight
//! and NULL none, once per placeholder that uses the value.
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{CachedStatement, Connection, Error, MappedRows, Result, Row, Rows};
use rusqlite::{Statement, ToSql};

/// Statements prepared for binding through [`Values`].
pub(crate) trait Prepare {
    /// Prepare `sql` through the connection's statement cache.
    fn statement<'s>(&self, sql: &'s str) -> Result<Bound<'_, 's>>;
    /// Prepare `sql` once, outside the statement cache.
    fn statement_once<'s>(&self, sql: &'s str) -> Result<Bound<'_, 's>>;
    /// `Connection::execute`: prepare `sql` once and run it.
    fn execute_once(&self, sql: &str, values: impl Values) -> Result<usize> {
        self.statement_once(sql)?.execute(values)
    }
    /// `Connection::query_row`: prepare `sql` once and read its first row.
    fn query_row_once<T>(
        &self,
        sql: &str,
        values: impl Values,
        f: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        self.statement_once(sql)?.query_row(values, f)
    }
}

impl Prepare for Connection {
    fn statement<'s>(&self, sql: &'s str) -> Result<Bound<'_, 's>> {
        Ok(Bound {
            statement: Held::Cached(self.prepare_cached(sql)?),
            sql,
            #[cfg(feature = "perf-witness")]
            raw: false,
        })
    }

    fn statement_once<'s>(&self, sql: &'s str) -> Result<Bound<'_, 's>> {
        Ok(Bound {
            statement: Held::Once(self.prepare(sql)?),
            sql,
            #[cfg(feature = "perf-witness")]
            raw: false,
        })
    }
}

enum Held<'c> {
    Cached(CachedStatement<'c>),
    Once(Statement<'c>),
}

/// A prepared statement whose executions bind through [`Values`]. The
/// binding methods the store uses are shadowed here; the rest reach the
/// underlying statement, so another binding method (`insert`, `exists`)
/// is shadowed here before its first use.
pub(crate) struct Bound<'c, 's> {
    statement: Held<'c>,
    sql: &'s str,
    /// Raw bindings since the last raw execution are being counted.
    #[cfg(feature = "perf-witness")]
    raw: bool,
}

impl<'c> std::ops::Deref for Bound<'c, '_> {
    type Target = Statement<'c>;

    fn deref(&self) -> &Statement<'c> {
        match &self.statement {
            Held::Cached(statement) => statement,
            Held::Once(statement) => statement,
        }
    }
}

impl<'c> std::ops::DerefMut for Bound<'c, '_> {
    fn deref_mut(&mut self) -> &mut Statement<'c> {
        match &mut self.statement {
            Held::Cached(statement) => statement,
            Held::Once(statement) => statement,
        }
    }
}

impl Bound<'_, '_> {
    /// Drop the statement from the cache instead of returning it.
    pub(crate) fn discard(self) {
        if let Held::Cached(statement) = self.statement {
            statement.discard();
        }
    }

    /// Whether an execution counts the value bytes it binds: a write with
    /// parameters while the witness collects.
    #[cfg(feature = "perf-witness")]
    fn counts(&self) -> bool {
        lash_core_ids::perf_witness::collecting() && !self.readonly() && self.parameter_count() > 0
    }

    /// Bind `values` and count the bytes a write binds.
    fn bind(&mut self, values: impl Values) -> Result<()> {
        #[cfg(feature = "perf-witness")]
        if self.counts() {
            return SIZES.with(|sizes| {
                let mut sizes = sizes.borrow_mut();
                sizes.clear();
                sizes.resize(self.parameter_count(), 0);
                values.bind(self, &mut Sizes(Some(&mut sizes)))?;
                lash_core_ids::perf_witness::record_sql_bytes_written(bound_bytes(
                    self.sql, self, &sizes,
                ));
                Ok(())
            });
        }
        values.bind(self, &mut Sizes(None))
    }

    /// Bind one value at its one-based `index`; [`Self::raw_execute`]
    /// counts what the raw bindings bound.
    pub(crate) fn raw_bind_parameter(&mut self, index: usize, value: impl ToSql) -> Result<()> {
        #[cfg(feature = "perf-witness")]
        if self.counts() {
            return SIZES.with(|sizes| {
                let mut sizes = sizes.borrow_mut();
                if !self.raw {
                    sizes.clear();
                    sizes.resize(self.parameter_count(), 0);
                    self.raw = true;
                }
                bind_one(self, index, &value, &mut Sizes(Some(&mut sizes)))
            });
        }
        bind_one(self, index, &value, &mut Sizes(None))
    }

    pub(crate) fn raw_execute(&mut self) -> Result<usize> {
        #[cfg(feature = "perf-witness")]
        if std::mem::take(&mut self.raw) {
            SIZES.with(|sizes| {
                lash_core_ids::perf_witness::record_sql_bytes_written(bound_bytes(
                    self.sql,
                    self,
                    &sizes.borrow(),
                ));
            });
        }
        Statement::raw_execute(self)
    }

    pub(crate) fn execute(&mut self, values: impl Values) -> Result<usize> {
        self.bind(values)?;
        Statement::raw_execute(self)
    }

    pub(crate) fn query(&mut self, values: impl Values) -> Result<Rows<'_>> {
        self.bind(values)?;
        Ok(Statement::raw_query(self))
    }

    pub(crate) fn query_row<T>(
        &mut self,
        values: impl Values,
        f: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut rows = self.query(values)?;
        rows.next()?.ok_or(Error::QueryReturnedNoRows).and_then(f)
    }

    pub(crate) fn query_map<T, F>(&mut self, values: impl Values, f: F) -> Result<MappedRows<'_, F>>
    where
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        Ok(self.query(values)?.mapped(f))
    }
}

#[cfg(feature = "perf-witness")]
thread_local! {
    /// Each parameter's value bytes for the write binding on this thread,
    /// reused across statements.
    static SIZES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Where binding records each parameter's value bytes, when counting.
pub(crate) struct Sizes<'a>(
    #[cfg_attr(not(feature = "perf-witness"), expect(dead_code))] Option<&'a mut Vec<u64>>,
);

/// Bind one value at its one-based `index`, converting it once.
fn bind_one(
    statement: &mut Statement<'_>,
    index: usize,
    value: &dyn ToSql,
    sizes: &mut Sizes<'_>,
) -> Result<()> {
    let value = value.to_sql()?;
    #[cfg(feature = "perf-witness")]
    if let Some(sizes) = &mut sizes.0
        && let Some(size) = sizes.get_mut(index.wrapping_sub(1))
    {
        *size = value_bytes(&value);
    }
    #[cfg(not(feature = "perf-witness"))]
    let _ = sizes;
    statement.raw_bind_parameter(index, value)
}

#[cfg(feature = "perf-witness")]
fn value_bytes(value: &ToSqlOutput<'_>) -> u64 {
    let value = match value {
        ToSqlOutput::Borrowed(value) => *value,
        ToSqlOutput::Owned(value) => ValueRef::from(value),
        _ => return 0,
    };
    match value {
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        // A text counts through its first NUL, as SQLite's expanded SQL
        // spells it (the counting semantics this replaced).
        ValueRef::Text(bytes) => bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len()) as u64,
        ValueRef::Blob(bytes) => bytes.len() as u64,
    }
}

/// The value bytes `sql` binds: every placeholder counts its parameter's
/// value, so a parameter used twice counts twice. A bare `?` takes the next
/// index after the largest so far, as SQLite numbers them.
#[cfg(feature = "perf-witness")]
fn bound_bytes(sql: &str, statement: &Statement<'_>, sizes: &[u64]) -> u64 {
    let mut largest = 0;
    let mut total = 0;
    for span in crate::observed_sql::placeholders(sql) {
        let spelling = &sql[span];
        let index = match spelling.strip_prefix('?') {
            Some("") => largest + 1,
            Some(digits) if digits.bytes().all(|b| b.is_ascii_digit()) => {
                digits.parse().unwrap_or_default()
            }
            _ => statement
                .parameter_index(spelling)
                .ok()
                .flatten()
                .unwrap_or_default(),
        };
        largest = largest.max(index);
        total += sizes
            .get(index.wrapping_sub(1))
            .copied()
            .unwrap_or_default();
    }
    total
}

/// Parameters a [`Bound`] statement binds: positional slices, arrays and
/// tuples, named slices, and [`params_from_iter`].
pub(crate) trait Values {
    fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()>;
}

fn bind_positional(
    statement: &mut Statement<'_>,
    values: &[&dyn ToSql],
    sizes: &mut Sizes<'_>,
) -> Result<()> {
    let expected = statement.parameter_count();
    if values.len() != expected {
        return Err(Error::InvalidParameterCount(values.len(), expected));
    }
    for (index, value) in values.iter().enumerate() {
        bind_one(statement, index + 1, *value, sizes)?;
    }
    Ok(())
}

impl Values for &[&dyn ToSql] {
    fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
        bind_positional(statement, self, sizes)
    }
}

impl Values for () {
    fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
        bind_positional(statement, &[], sizes)
    }
}

impl Values for [&(dyn ToSql + Send + Sync); 0] {
    fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
        bind_positional(statement, &[], sizes)
    }
}

impl Values for &[(&str, &dyn ToSql)] {
    fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
        for (name, value) in self {
            let index = statement
                .parameter_index(name)?
                .ok_or_else(|| Error::InvalidParameterName((*name).to_owned()))?;
            bind_one(statement, index, *value, sizes)?;
        }
        Ok(())
    }
}

macro_rules! array_values {
    ($($n:literal)+) => {$(
        impl<T: ToSql> Values for [T; $n] {
            fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
                let values: [&dyn ToSql; $n] = std::array::from_fn(|index| &self[index] as &dyn ToSql);
                bind_positional(statement, &values, sizes)
            }
        }
        impl<T: ToSql> Values for &[T; $n] {
            fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
                let values: [&dyn ToSql; $n] = std::array::from_fn(|index| &self[index] as &dyn ToSql);
                bind_positional(statement, &values, sizes)
            }
        }
    )+};
}
array_values!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16);

macro_rules! tuple_values {
    ($(($($name:ident $index:tt),+))+) => {$(
        impl<$($name: ToSql),+> Values for ($($name,)+) {
            fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
                bind_positional(statement, &[$(&self.$index as &dyn ToSql),+], sizes)
            }
        }
    )+};
}
tuple_values! {
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
    (A 0, B 1, C 2, D 3, E 4)
    (A 0, B 1, C 2, D 3, E 4, F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11)
}

/// Positional values from an iterator, as `rusqlite::params_from_iter`.
pub(crate) struct FromIter<I>(I);

pub(crate) fn params_from_iter<I>(values: I) -> FromIter<I>
where
    I: IntoIterator,
    I::Item: ToSql,
{
    FromIter(values)
}

impl<I> Values for FromIter<I>
where
    I: IntoIterator,
    I::Item: ToSql,
{
    fn bind(self, statement: &mut Statement<'_>, sizes: &mut Sizes<'_>) -> Result<()> {
        let expected = statement.parameter_count();
        let mut count = 0;
        for value in self.0 {
            count += 1;
            if count > expected {
                break;
            }
            bind_one(statement, count, &value, sizes)?;
        }
        if count != expected {
            return Err(Error::InvalidParameterCount(count, expected));
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "perf-witness"))]
mod tests {
    use super::*;

    /// SQL-BYTES-WRITTEN: a write counts the bytes of the values it binds,
    /// not their SQL spelling: a text without its quoting, a blob without its
    /// hex, eight per number, none for NULL, and every use of a repeated
    /// parameter; placeholders inside comments and quoted text are not
    /// parameters. The replay gate's depth slopes bind this count because it
    /// does not move with B-tree page layout (FIG-6280).
    #[test]
    fn a_write_counts_the_bytes_of_the_values_it_binds() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE sample (a, b, c, d, e, f)")
            .unwrap();
        let sql = "INSERT INTO sample VALUES (?1, :blob, ?3, -- ?9\n ?4, '?', ?1)";
        let mut statement = connection.statement_once(sql).unwrap();
        let mut sizes = vec![0; statement.parameter_count()];
        rusqlite::params![
            "it's é",
            &[0_u8, 1, 2, 3, 4][..],
            -12.5_f64,
            rusqlite::types::Null
        ]
        .bind(&mut statement, &mut Sizes(Some(&mut sizes)))
        .unwrap();
        // 7 text bytes twice, 5 blob bytes, one number, NULL.
        assert_eq!(bound_bytes(sql, &statement, &sizes), 7 * 2 + 5 + 8);
    }
}
