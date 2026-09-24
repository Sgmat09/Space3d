//! serde `Deserializer`s mapping SQL rows/values onto Rust types.

use crate::error::DbError;
use crate::value::Value;
use serde::de::value::{SeqDeserializer, StrDeserializer};
use serde::de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};
use serde::forward_to_deserialize_any;

type Result<T> = std::result::Result<T, DbError>;

/// Deserializes one row: struct/map by column name, tuple/seq by position,
/// scalar when the row has exactly one column.
pub(crate) struct RowDeserializer<'de> {
    columns: &'de [String],
    values: &'de [Value],
}

impl<'de> RowDeserializer<'de> {
    pub(crate) fn new(columns: &'de [String], values: &'de [Value]) -> Self {
        debug_assert_eq!(columns.len(), values.len());
        Self { columns, values }
    }

    fn single(&self) -> Result<ValueDeserializer<'de>> {
        match self.values {
            [v] => Ok(ValueDeserializer(v)),
            other => Err(de::Error::custom(format_args!(
                "expected exactly one column to deserialize a scalar, got {}",
                other.len()
            ))),
        }
    }
}

macro_rules! row_scalar {
    ($($m:ident),*) => {$(
        fn $m<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            self.single()?.$m(visitor)
        }
    )*};
}

impl<'de> de::Deserializer<'de> for RowDeserializer<'de> {
    type Error = DbError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_map(RowMap {
            columns: self.columns,
            values: self.values,
            idx: 0,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_seq(ValueSeq {
            values: self.values.iter(),
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.values {
            [] => visitor.visit_none(),
            [v] => ValueDeserializer(v).deserialize_option(visitor),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.single()?.deserialize_enum(name, variants, visitor)
    }

    row_scalar!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_identifier
    );
}

struct RowMap<'de> {
    columns: &'de [String],
    values: &'de [Value],
    idx: usize,
}

impl<'de> MapAccess<'de> for RowMap<'de> {
    type Error = DbError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        match self.columns.get(self.idx) {
            Some(name) => seed.deserialize(StrDeserializer::<DbError>::new(name)).map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let v = &self.values[self.idx];
        self.idx += 1;
        seed.deserialize(ValueDeserializer(v))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.columns.len() - self.idx)
    }
}

struct ValueSeq<'de> {
    values: std::slice::Iter<'de, Value>,
}

impl<'de> SeqAccess<'de> for ValueSeq<'de> {
    type Error = DbError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        self.values
            .next()
            .map(|v| seed.deserialize(ValueDeserializer(v)))
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.values.len())
    }
}

/// Deserializes a single SQL value.
pub(crate) struct ValueDeserializer<'de>(pub(crate) &'de Value);

/// Run a serde_json deserializer method over JSON text stored in a column.
macro_rules! from_json {
    ($s:expr, |$de:ident| $call:expr) => {{
        let mut $de = serde_json::Deserializer::from_str($s);
        let out = $call.map_err(DbError::from)?;
        $de.end().map_err(DbError::from)?;
        Ok(out)
    }};
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'de> {
    type Error = DbError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Integer(i) => visitor.visit_i64(*i),
            Value::Real(f) => visitor.visit_f64(*f),
            Value::Text(s) => visitor.visit_borrowed_str(s),
            Value::Blob(b) => visitor.visit_borrowed_bytes(b),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.0 {
            Value::Integer(i) => visitor.visit_bool(*i != 0),
            Value::Text(s) if s.eq_ignore_ascii_case("true") => visitor.visit_bool(true),
            Value::Text(s) if s.eq_ignore_ascii_case("false") => visitor.visit_bool(false),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.0 {
            Value::Text(s) => visitor.visit_borrowed_bytes(s.as_bytes()),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.0 {
            // `Vec<u8>` deserializes through `deserialize_seq`.
            Value::Blob(b) => visitor.visit_seq(SeqDeserializer::<_, DbError>::new(b.iter().copied())),
            Value::Text(s) => from_json!(s, |de| de::Deserializer::deserialize_seq(&mut de, visitor)),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        match self.0 {
            Value::Text(s) => from_json!(s, |de| de::Deserializer::deserialize_tuple(&mut de, len, visitor)),
            _ => self.deserialize_seq(visitor),
        }
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.0 {
            Value::Text(s) => from_json!(s, |de| de::Deserializer::deserialize_map(&mut de, visitor)),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.0 {
            Value::Text(s) => from_json!(s, |de| de::Deserializer::deserialize_struct(
                &mut de, name, fields, visitor
            )),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.0 {
            Value::Text(s) if s.starts_with('{') || s.starts_with('"') => {
                from_json!(s, |de| de::Deserializer::deserialize_enum(
                    &mut de, name, variants, visitor
                ))
            }
            Value::Text(s) => visitor.visit_enum(s.as_str().into_deserializer()),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        unit unit_struct identifier
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn row<T: serde::de::DeserializeOwned>(cols: &[&str], vals: Vec<Value>) -> Result<T> {
        let cols: Vec<String> = cols.iter().map(|s| s.to_string()).collect();
        T::deserialize(RowDeserializer::new(&cols, &vals))
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Inner {
        a: i32,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    enum Kind {
        Warrior,
        Mage,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Player {
        id: i64,
        name: String,
        alive: bool,
        hp: f32,
        nick: Option<String>,
        avatar: Vec<u8>,
        inner: Inner,
        tags: Vec<String>,
        kind: Kind,
    }

    #[test]
    fn struct_by_column_name() {
        let p: Player = row(
            &["name", "id", "alive", "hp", "nick", "avatar", "inner", "tags", "kind"],
            vec![
                Value::Text("bob".into()),
                Value::Integer(7),
                Value::Integer(1),
                Value::Integer(10),
                Value::Null,
                Value::Blob(vec![1, 2]),
                Value::Text(r#"{"a":3}"#.into()),
                Value::Text(r#"["x","y"]"#.into()),
                Value::Text("Mage".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            p,
            Player {
                id: 7,
                name: "bob".into(),
                alive: true,
                hp: 10.0,
                nick: None,
                avatar: vec![1, 2],
                inner: Inner { a: 3 },
                tags: vec!["x".into(), "y".into()],
                kind: Kind::Mage,
            }
        );
        let _ = Kind::Warrior;
    }

    #[test]
    fn scalars_tuples_and_errors() {
        assert_eq!(row::<i64>(&["n"], vec![Value::Integer(5)]).unwrap(), 5);
        assert_eq!(row::<Option<i64>>(&["n"], vec![Value::Null]).unwrap(), None);
        assert_eq!(
            row::<(i64, String)>(&["a", "b"], vec![Value::Integer(1), Value::Text("x".into())]).unwrap(),
            (1, "x".into())
        );
        assert!(row::<i64>(&["a", "b"], vec![Value::Integer(1), Value::Integer(2)]).is_err());
        assert!(matches!(
            row::<i64>(&["a"], vec![Value::Text("nope".into())]),
            Err(DbError::Serde(_))
        ));
    }
}
