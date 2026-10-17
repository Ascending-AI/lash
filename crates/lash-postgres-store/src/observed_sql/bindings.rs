//! Counts the bytes the real encoder appends, without a second encode or
//! retaining values in ordinary builds. The SQL-shape observer keeps bounded
//! equality keys in its collector; queries carry only an epoch/token.
use futures_util::stream::{BoxStream, StreamExt, TryStreamExt};
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgArguments, PgRow, PgStatement, PgTypeInfo};
use sqlx::{Database, Encode, Execute, Executor, FromRow, Postgres, Type};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[cfg(feature = "perf-witness")]
type Exact = Option<lash_core_execution::perf_witness::sql_shapes::work::BindingCapture>;
#[cfg(feature = "perf-witness")]
type BindingEvidence = (Option<u64>, Option<Vec<u8>>);

type Plain<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;
pub(crate) type Query<'q> = Bound<Plain<'q>>;

pub(crate) struct Bound<Q> {
    inner: Q,
    bytes: Option<Arc<AtomicU64>>,
    unmeasured_binds: bool,
    #[cfg(feature = "perf-witness")]
    exact: Exact,
}
impl<Q> Bound<Q> {
    fn new(inner: Q) -> Self {
        Self {
            inner,
            bytes: (lash_core_execution::facade_support::sql::Window::current().is_some()
                || cfg!(any(test, feature = "perf-witness")))
            .then(Arc::default),
            unmeasured_binds: false,
            #[cfg(feature = "perf-witness")]
            exact: lash_core_execution::perf_witness::sql_shapes::work::BindingCapture::new(),
        }
    }
}
struct Value<T> {
    inner: T,
    bytes: Option<Arc<AtomicU64>>,
    #[cfg(feature = "perf-witness")]
    exact: Exact,
}
impl<T: Type<Postgres>> Type<Postgres> for Value<T> {
    fn type_info() -> PgTypeInfo {
        T::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        T::compatible(ty)
    }
}
impl<'q, T: Encode<'q, Postgres> + Type<Postgres>> Encode<'q, Postgres> for Value<T> {
    fn encode_by_ref(
        &self,
        buf: &mut PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, BoxDynError> {
        let before = buf.len();
        let result = self.inner.encode_by_ref(buf)?;
        #[cfg(feature = "perf-witness")]
        capture_value::<T>(&self.exact, &buf[before..], &result);
        if let Some(bytes) = &self.bytes {
            bytes.fetch_add((buf.len() - before) as u64, Ordering::Relaxed);
        }
        Ok(result)
    }
    fn encode(self, buf: &mut PgArgumentBuffer) -> Result<sqlx::encode::IsNull, BoxDynError> {
        let before = buf.len();
        let result = self.inner.encode(buf)?;
        #[cfg(feature = "perf-witness")]
        capture_value::<T>(&self.exact, &buf[before..], &result);
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
        #[cfg(feature = "perf-witness")]
        report_bindings(
            self.bytes.as_ref().map(|b| b.load(Ordering::Relaxed)),
            self.exact.and_then(|capture| capture.take()),
        );
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
                #[cfg(feature = "perf-witness")]
                exact: self.exact,
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
                #[cfg(feature = "perf-witness")]
                exact: self.exact,
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
                #[cfg(feature = "perf-witness")]
                exact: self.exact,
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

#[cfg(feature = "perf-witness")]
thread_local! {
    static BINDINGS: std::cell::RefCell<Option<BindingEvidence>> = const { std::cell::RefCell::new(None) };
    static CAPTURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(feature = "perf-witness")]
fn report_bindings(bytes: Option<u64>, identity: Option<Vec<u8>>) {
    if CAPTURING.with(std::cell::Cell::get) {
        BINDINGS.with(|v| *v.borrow_mut() = Some((bytes, identity)));
    }
}
#[cfg(feature = "perf-witness")]
pub(super) fn capture<T>(work: impl FnOnce() -> T) -> (T, Option<BindingEvidence>) {
    struct Restore(Option<BindingEvidence>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            BINDINGS.with(|v| *v.borrow_mut() = self.0.take());
            CAPTURING.with(|v| v.set(self.1));
        }
    }
    let _restore = Restore(
        BINDINGS.with(|v| v.replace(None)),
        CAPTURING.with(|v| v.replace(true)),
    );
    let result = work();
    (result, BINDINGS.with(|v| v.borrow_mut().take()))
}
#[cfg(feature = "perf-witness")]
fn capture_value<T: Type<Postgres>>(exact: &Exact, bytes: &[u8], null: &sqlx::encode::IsNull) {
    use sqlx::TypeInfo as _;
    let Some(exact) = exact else {
        return;
    };
    // Only known built-in encoders are patch-free here. Do not call a
    // user-provided produces()/encoder again for observation.
    if !exact_rust_type(std::any::type_name::<T>()) {
        exact.append("", &[], false, false);
        return;
    }
    let ty = T::type_info();
    let name = ty.name();
    let builtin = matches!(
        name,
        "BOOL"
            | "INT2"
            | "INT4"
            | "INT8"
            | "FLOAT4"
            | "FLOAT8"
            | "TEXT"
            | "VARCHAR"
            | "BYTEA"
            | "JSON"
            | "JSONB"
            | "UUID"
            | "TIMESTAMP"
            | "TIMESTAMPTZ"
    );
    exact.append(
        name,
        bytes,
        matches!(null, sqlx::encode::IsNull::Yes),
        builtin,
    );
}

#[cfg(feature = "perf-witness")]
fn exact_rust_type(name: &str) -> bool {
    if let Some(inner) = name.strip_prefix('&') {
        return exact_rust_type(inner);
    }
    if let Some(inner) = name
        .strip_prefix("core::option::Option<")
        .and_then(|s| s.strip_suffix('>'))
    {
        return exact_rust_type(inner);
    }
    matches!(
        name,
        "bool"
            | "i16"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
            | "str"
            | "alloc::string::String"
            | "[u8]"
            | "alloc::vec::Vec<u8>"
            | "uuid::Uuid"
    )
}
