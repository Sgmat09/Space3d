use crate::de::RowDeserializer;
use crate::error::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// An owned SQLite value, used for query parameters and raw result rows.
///
/// Parameters are owned because they travel to the database worker thread.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// SQL `NULL`.
    #[default]
    Null,
    /// 64-bit signed integer (also used for `bool`: 0/1).
    Integer(i64),
    /// 64-bit float.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Binary blob.
    Blob(Vec<u8>),
}

impl Value {
    /// Serialize any value to JSON text (handy for JSON columns; the row
    /// deserializer parses JSON text back into structs/maps/sequences).
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        Ok(Self::Text(serde_json::to_string(value)?))
    }

    /// `true` for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Integer payload, if this is an integer.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Numeric payload as `f64` (integers are converted).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Integer(i) => Some(*i as f64),
            Value::Real(f) => Some(*f),
            _ => None,
        }
    }

    /// Text payload, if this is text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Blob payload, if this is a blob.
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Self { Value::Integer(i64::from(v)) }
        }
    )*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Integer(v as i64)
    }
}
impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Value::Real(f64::from(v))
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Value::Text(v.clone())
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Blob(v)
    }
}
impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Value::Blob(v.to_vec())
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// Anything usable as positional (`?1`, `?2`, ...) query parameters.
///
/// Implemented for `()`, tuples of up to 12 `Into<Value>` items, `Vec<T>`,
/// arrays and slices. Use [`crate::params!`] for heterogeneous lists.
///
/// ```
/// # use space3d_db::{IntoParams, Value};
/// assert_eq!(().into_params(), vec![]);
/// assert_eq!((1, "a").into_params(), vec![Value::Integer(1), Value::Text("a".into())]);
/// ```
pub trait IntoParams {
    /// Convert into an owned parameter list.
    fn into_params(self) -> Vec<Value>;
}

impl IntoParams for () {
    fn into_params(self) -> Vec<Value> {
        Vec::new()
    }
}
impl<T: Into<Value>> IntoParams for Vec<T> {
    fn into_params(self) -> Vec<Value> {
        self.into_iter().map(Into::into).collect()
    }
}
impl<T: Into<Value>, const N: usize> IntoParams for [T; N] {
    fn into_params(self) -> Vec<Value> {
        self.into_iter().map(Into::into).collect()
    }
}
impl<T: Into<Value> + Clone> IntoParams for &[T] {
    fn into_params(self) -> Vec<Value> {
        self.iter().cloned().map(Into::into).collect()
    }
}

macro_rules! tuple_params {
    ($($name:ident),+) => {
        impl<$($name: Into<Value>),+> IntoParams for ($($name,)+) {
            #[allow(non_snake_case)]
            fn into_params(self) -> Vec<Value> {
                let ($($name,)+) = self;
                vec![$($name.into()),+]
            }
        }
    };
}
tuple_params!(A);
tuple_params!(A, B);
tuple_params!(A, B, C);
tuple_params!(A, B, C, D);
tuple_params!(A, B, C, D, E);
tuple_params!(A, B, C, D, E, F);
tuple_params!(A, B, C, D, E, F, G);
tuple_params!(A, B, C, D, E, F, G, H);
tuple_params!(A, B, C, D, E, F, G, H, I);
tuple_params!(A, B, C, D, E, F, G, H, I, J);
tuple_params!(A, B, C, D, E, F, G, H, I, J, K);
tuple_params!(A, B, C, D, E, F, G, H, I, J, K, L);

/// Build a heterogeneous `Vec<Value>` parameter list.
///
/// ```
/// use space3d_db::{params, Value};
/// let p = params![1, "two", 3.0, None::<i64>];
/// assert_eq!(p[3], Value::Null);
/// ```
#[macro_export]
macro_rules! params {
    () => { ::std::vec::Vec::<$crate::Value>::new() };
    ($($v:expr),+ $(,)?) => { ::std::vec![$($crate::Value::from($v)),+] };
}

/// A fully materialized result set (column names + rows of [`Value`]s).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    /// Column names, in select order.
    pub columns: Vec<String>,
    /// Row values; each row has `columns.len()` entries.
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    /// Number of rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// `true` when there are no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Borrowing iterator over rows.
    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|values| Row {
            columns: &self.columns,
            values,
        })
    }

    /// Deserialize every row (see [`Row::deserialize`]).
    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<Vec<T>> {
        self.iter().map(|r| r.deserialize()).collect()
    }
}

/// One borrowed row of a [`Rows`] result.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    columns: &'a [String],
    values: &'a [Value],
}

impl<'a> Row<'a> {
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) fn new(columns: &'a [String], values: &'a [Value]) -> Self {
        Self { columns, values }
    }

    /// Column names.
    pub fn columns(&self) -> &'a [String] {
        self.columns
    }

    /// Value of the column called `name`.
    pub fn get(&self, name: &str) -> Option<&'a Value> {
        self.columns.iter().position(|c| c == name).map(|i| &self.values[i])
    }

    /// Values in column order.
    pub fn values(&self) -> &'a [Value] {
        self.values
    }

    /// Map the row onto `T` via serde, matching struct fields to column names.
    ///
    /// * structs / maps: by column name
    /// * tuples / sequences: by column position
    /// * scalars (`i64`, `String`, ...): the row must have exactly one column
    /// * `bool` accepts integers (0/1), `Option<T>` maps `NULL` to `None`
    /// * `Vec<u8>` reads blobs; nested structs / maps / sequences / enums are
    ///   parsed from JSON text columns.
    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<T> {
        T::deserialize(RowDeserializer::new(self.columns, self.values))
    }
}
