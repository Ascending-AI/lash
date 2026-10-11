//! Counts the bytes the real encoder appends, without a second encode or
//! retaining values. Carry the count with the query until it is executed.
use futures_util::stream::{BoxStream, StreamExt, TryStreamExt};
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgArguments, PgRow, PgStatement, PgTypeInfo};
use sqlx::{Database, Encode, Execute, Executor, FromRow, Postgres, Type};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

type Plain<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;
pub(crate) type Query<'q> = Bound<Plain<'q>>;

pub(crate) struct Bound<Q> {
    inner: Q,
    bytes: Option<Arc<AtomicU64>>,
    unmeasured_binds: bool,
}
impl<Q> Bound<Q> {
    fn new(inner: Q) -> Self {
        Self {
            inner,
            bytes: (lash_core_execution::facade_support::sql::Window::current().is_some()
                || cfg!(any(test, feature = "perf-witness")))
            .then(Arc::default),
            unmeasured_binds: false,
        }
    }
}
struct Value<T> {
    inner: T,
    bytes: Option<Arc<AtomicU64>>,
}
impl<T: Type<Postgres>> Type<Postgres> for Value<T> {
    fn type_info() -> PgTypeInfo {
        T::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        T::compatible(ty)
    }
}
impl<'q, T: Encode<'q, Postgres>> Encode<'q, Postgres> for Value<T> {
    fn encode_by_ref(
        &self,
        buf: &mut PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, BoxDynError> {
        let before = buf.len();
        let result = self.inner.encode_by_ref(buf)?;
        if let Some(bytes) = &self.bytes {
            bytes.fetch_add((buf.len() - before) as u64, Ordering::Relaxed);
        }
        Ok(result)
    }
    fn encode(self, buf: &mut PgArgumentBuffer) -> Result<sqlx::encode::IsNull, BoxDynError> {
        let before = buf.len();
        let result = self.inner.encode(buf)?;
        if let Some(bytes) = &self.bytes {
            bytes.fetch_add((buf.len() - before) as u64, Ordering::Relaxed);
        }
        Ok(result)
    }
    fn produces(&self) -> Option<PgTypeInfo> {
        self.inner.produces()
    }
    fn size_hint(&self) -> usize {
        self.inner.size_hint()
    }
}
impl<'q, Q: Execute<'q, Postgres>> Execute<'q, Postgres> for Bound<Q> {
    fn sql(&self) -> &'q str {
        self.inner.sql()
    }
    fn statement(&self) -> Option<&PgStatement<'q>> {
        self.inner.statement()
    }
    fn take_arguments(&mut self) -> Result<Option<PgArguments>, BoxDynError> {
        if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
            window.payload(
                self.inner.sql(),
                0,
                self.bytes
                    .as_ref()
                    .map_or(0, |bytes| bytes.swap(0, Ordering::Relaxed)),
                0,
            );
        }
        let arguments = self.inner.take_arguments()?;
        if arguments.is_some() && self.unmeasured_binds {
            if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
                window.unmeasured_bound(self.inner.sql());
            }
            self.unmeasured_binds = false;
        }
        Ok(arguments)
    }
    fn persistent(&self) -> bool {
        self.inner.persistent()
    }
}

pub(crate) trait Output {
    type Item: Send + Unpin;
    fn decode(row: PgRow) -> Result<Self::Item, sqlx::Error>;
}
impl Output for Plain<'_> {
    type Item = PgRow;
    fn decode(row: PgRow) -> Result<PgRow, sqlx::Error> {
        Ok(row)
    }
}
impl<O: Send + Unpin + for<'r> FromRow<'r, PgRow>> Output
    for sqlx::query::QueryAs<'_, Postgres, O, PgArguments>
{
    type Item = O;
    fn decode(row: PgRow) -> Result<O, sqlx::Error> {
        O::from_row(&row)
    }
}
impl<O: Send + Unpin> Output for sqlx::query::QueryScalar<'_, Postgres, O, PgArguments>
where
    (O,): for<'r> FromRow<'r, PgRow>,
{
    type Item = O;
    fn decode(row: PgRow) -> Result<O, sqlx::Error> {
        <(O,)>::from_row(&row).map(|(value,)| value)
    }
}

impl<'q, Q: Execute<'q, Postgres> + Output + 'q> Bound<Q> {
    pub(crate) async fn execute<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error>
    where
        'q: 'e,
        Q: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        executor.execute(self).await
    }
    pub(crate) async fn fetch_all<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> Result<Vec<Q::Item>, sqlx::Error>
    where
        'q: 'e,
        Q: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        self.fetch(executor).try_collect().await
    }
    pub(crate) async fn fetch_one<'e, 'c: 'e, E>(self, executor: E) -> Result<Q::Item, sqlx::Error>
    where
        'q: 'e,
        Q: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        Q::decode(executor.fetch_one(self).await?)
    }
    pub(crate) async fn fetch_optional<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> Result<Option<Q::Item>, sqlx::Error>
    where
        'q: 'e,
        Q: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        executor
            .fetch_optional(self)
            .await?
            .map(Q::decode)
            .transpose()
    }
    pub(crate) fn fetch<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> BoxStream<'e, Result<Q::Item, sqlx::Error>>
    where
        'q: 'e,
        Q: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        Box::pin(executor.fetch(self).map(|row| row.and_then(Q::decode)))
    }
}
impl<'q> Bound<Plain<'q>> {
    pub(crate) fn bind<T: 'q + Encode<'q, Postgres> + Type<Postgres>>(mut self, value: T) -> Self {
        self.inner = if self.bytes.is_some() {
            self.inner.bind(Value {
                inner: value,
                bytes: self.bytes.clone(),
            })
        } else {
            self.unmeasured_binds = true;
            self.inner.bind(value)
        };
        self
    }
}
impl<'q, O> Bound<sqlx::query::QueryAs<'q, Postgres, O, PgArguments>> {
    pub(crate) fn bind<T: 'q + Encode<'q, Postgres> + Type<Postgres>>(mut self, value: T) -> Self {
        self.inner = if self.bytes.is_some() {
            self.inner.bind(Value {
                inner: value,
                bytes: self.bytes.clone(),
            })
        } else {
            self.unmeasured_binds = true;
            self.inner.bind(value)
        };
        self
    }
}
impl<'q, O> Bound<sqlx::query::QueryScalar<'q, Postgres, O, PgArguments>> {
    pub(crate) fn bind<T: 'q + Encode<'q, Postgres> + Type<Postgres>>(mut self, value: T) -> Self {
        self.inner = if self.bytes.is_some() {
            self.inner.bind(Value {
                inner: value,
                bytes: self.bytes.clone(),
            })
        } else {
            self.unmeasured_binds = true;
            self.inner.bind(value)
        };
        self
    }
}
pub(crate) fn query(sql: &str) -> Query<'_> {
    Bound::new(sqlx::query(sql))
}
pub(crate) fn query_as<'q, DB: Database, O: for<'r> FromRow<'r, DB::Row>>(
    sql: &'q str,
) -> Bound<sqlx::query::QueryAs<'q, DB, O, DB::Arguments<'q>>> {
    Bound::new(sqlx::query_as(sql))
}
pub(crate) fn query_scalar<'q, DB: Database, O>(
    sql: &'q str,
) -> Bound<sqlx::query::QueryScalar<'q, DB, O, DB::Arguments<'q>>>
where
    (O,): for<'r> FromRow<'r, DB::Row>,
{
    Bound::new(sqlx::query_scalar(sql))
}
