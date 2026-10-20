//! PostgreSQL's content-address boundary: domain references remain hexadecimal,
//! while SQL parameters, arrays and columns carry the raw 32-byte digest.
use sqlx::postgres::{PgArgumentBuffer, PgHasArrayType, PgRow, PgTypeInfo, PgValueRef};
use sqlx::{ColumnIndex, Decode, Encode, Executor, Postgres, Row, Type};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key(pub(crate) String);

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for Key {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

pub(crate) fn key(value: impl AsRef<str>) -> Key {
    Key(value.as_ref().to_owned())
}

/// A lookup outside the digest key space names no stored row. Bind NULL for
/// that case so reads retain their absent-reference result.
pub(crate) fn lookup(value: impl AsRef<str>) -> Option<Key> {
    lash_core_store::checkpoint_graph::NodeId::from_hex(value.as_ref())?;
    Some(key(value))
}

pub(crate) fn keys(values: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<Key> {
    values.into_iter().map(key).collect()
}

impl Type<Postgres> for Key {
    fn type_info() -> PgTypeInfo {
        <Vec<u8> as Type<Postgres>>::type_info()
    }
}
impl PgHasArrayType for Key {
    fn array_type_info() -> PgTypeInfo {
        <Vec<u8> as PgHasArrayType>::array_type_info()
    }
}
impl<'q> Encode<'q, Postgres> for Key {
    fn encode_by_ref(
        &self,
        buffer: &mut PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        let node = lash_core_store::checkpoint_graph::NodeId::from_hex(&self.0)
            .ok_or("content address must be a 32-byte hexadecimal digest")?;
        <&[u8] as Encode<Postgres>>::encode(&node.0[..], buffer)
    }
    fn size_hint(&self) -> usize {
        32
    }
}
impl<'r> Decode<'r, Postgres> for Key {
    fn decode(value: PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let bytes = <Vec<u8> as Decode<Postgres>>::decode(value)?;
        let digest = <[u8; 32]>::try_from(bytes.as_slice())?;
        Ok(Self(
            lash_core_store::checkpoint_graph::NodeId(digest).to_hex(),
        ))
    }
}

/// The shapes in which a content-key column crosses back into the domain.
pub(crate) trait FromKey: Sized {
    type Stored: for<'r> Decode<'r, Postgres> + Type<Postgres> + Send + Unpin + 'static;
    fn from_key(stored: Self::Stored) -> Self;
}
impl FromKey for String {
    type Stored = Key;
    fn from_key(stored: Key) -> Self {
        stored.0
    }
}
impl<T: FromKey> FromKey for Option<T> {
    type Stored = Option<T::Stored>;
    fn from_key(stored: Self::Stored) -> Self {
        stored.map(T::from_key)
    }
}
impl FromKey for Vec<String> {
    type Stored = Vec<Key>;
    fn from_key(stored: Self::Stored) -> Self {
        stored.into_iter().map(|key| key.0).collect()
    }
}

pub(crate) fn get<T: FromKey>(
    row: &PgRow,
    index: impl ColumnIndex<PgRow>,
) -> Result<T, sqlx::Error> {
    row.try_get(index).map(T::from_key)
}

/// A scalar content-key read. Decoding stays here instead of spreading the
/// database representation through graph, checkpoint and prompt algorithms.
pub(crate) struct Query<'q, T: FromKey> {
    inner: crate::observed_sql::bindings::Bound<
        sqlx::query::QueryScalar<'q, Postgres, T::Stored, sqlx::postgres::PgArguments>,
    >,
}
pub(crate) fn query_scalar<T: FromKey>(sql: &str) -> Query<'_, T> {
    Query {
        inner: crate::observed_sql::query_scalar(sql),
    }
}
impl<'q, T: FromKey> Query<'q, T> {
    pub(crate) fn bind<V: 'q + Encode<'q, Postgres> + Type<Postgres>>(mut self, value: V) -> Self {
        self.inner = self.inner.bind(value);
        self
    }
    pub(crate) async fn fetch_all<'e, 'c: 'e, E>(self, executor: E) -> Result<Vec<T>, sqlx::Error>
    where
        'q: 'e,
        T::Stored: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        self.inner
            .fetch_all(executor)
            .await
            .map(|values| values.into_iter().map(T::from_key).collect())
    }
    pub(crate) async fn fetch_one<'e, 'c: 'e, E>(self, executor: E) -> Result<T, sqlx::Error>
    where
        'q: 'e,
        T::Stored: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        self.inner.fetch_one(executor).await.map(T::from_key)
    }
    pub(crate) async fn fetch_optional<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> Result<Option<T>, sqlx::Error>
    where
        'q: 'e,
        T::Stored: 'e,
        E: Executor<'c, Database = Postgres>,
    {
        self.inner
            .fetch_optional(executor)
            .await
            .map(|value| value.map(T::from_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Content-key reads preserve the domain digest, nulls and array order;
    /// the PostgreSQL parameter carries exactly the raw digest bytes.
    #[tokio::test]
    async fn content_key_reads_round_trip_when_configured() {
        let url = crate::postgres_test_support::database_url().expect("hermetic PostgreSQL");
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        let first = lash_core_store::checkpoint_graph::NodeId([0; 32]).to_hex();
        let second = lash_core_store::checkpoint_graph::NodeId([255; 32]).to_hex();
        let width: i32 = crate::observed_sql::query_scalar("SELECT octet_length($1::bytea)")
            .bind(key(&first))
            .fetch_one(&pool)
            .await
            .expect("binary width");
        assert_eq!(width, 32);
        let scalar: String = query_scalar("SELECT $1::bytea")
            .bind(key(&second))
            .fetch_one(&pool)
            .await
            .expect("scalar");
        assert_eq!(scalar, second);
        let nullable: Option<String> = query_scalar("SELECT $1::bytea")
            .bind(None::<Key>)
            .fetch_one(&pool)
            .await
            .expect("nullable");
        assert_eq!(nullable, None);
        let absent: Option<String> = query_scalar("SELECT $1::bytea WHERE FALSE")
            .bind(key(&first))
            .fetch_optional(&pool)
            .await
            .expect("absent");
        assert_eq!(absent, None);
        let array: Vec<String> = query_scalar("SELECT $1::bytea[]")
            .bind(keys([&second, &first]))
            .fetch_one(&pool)
            .await
            .expect("array");
        assert_eq!(array, [second.clone(), first.clone()]);
        let rows: Vec<String> = query_scalar("SELECT unnest($1::bytea[])")
            .bind(keys([&first, &second]))
            .fetch_all(&pool)
            .await
            .expect("rows");
        assert_eq!(rows, [first, second]);
        let corrupt = query_scalar::<String>("SELECT '\\x00'::bytea")
            .fetch_one(&pool)
            .await
            .expect_err("short digest refused");
        assert!(matches!(corrupt, sqlx::Error::ColumnDecode { .. }));
    }
}
