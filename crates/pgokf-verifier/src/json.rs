//! JSON contract input that refuses duplicate keys at every nesting level.
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::fmt;

struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor)
    }
}
struct UniqueVisitor;
impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = Unique;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
        Ok(Unique(Value::Bool(v)))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
        Ok(Unique(Value::from(v)))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
        Ok(Unique(Value::from(v)))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Unique, E> {
        serde_json::Number::from_f64(v)
            .map(|v| Unique(Value::Number(v)))
            .ok_or_else(|| E::custom("nonfinite JSON number"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
        Ok(Unique(Value::String(v.into())))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
        Ok(Unique(Value::Null))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
        let mut values = vec![];
        while let Some(Unique(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(Unique(Value::Array(values)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Unique, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
            if values.insert(key.clone(), value).is_some() {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key: {key}"
                )));
            }
        }
        Ok(Unique(Value::Object(values)))
    }
}
/// Deserialize a typed contract after recursively enforcing unique JSON keys.
///
/// # Errors
/// Invalid JSON, duplicate keys, trailing input and type mismatches are rejected.
pub fn from_slice<T: DeserializeOwned>(input: &[u8]) -> Result<T, String> {
    let Unique(value) = serde_json::from_slice(input).map_err(|e| e.to_string())?;
    serde_json::from_value(value).map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_and_escaped_duplicate_keys_are_rejected() {
        for input in [
            r#"{"a":1,"a":2}"#,
            r#"[{"a":1,"\u0061":2}]"#,
            r#"{"x":{"a":null,"a":true}}"#,
        ] {
            assert!(
                from_slice::<Value>(input.as_bytes())
                    .unwrap_err()
                    .contains("duplicate JSON key")
            );
        }
    }
    #[test]
    fn independent_objects_and_all_value_kinds_survive() {
        let input = br#"[{"a":1},{"a":-1},true,null,"x",1.5,18446744073709551615]"#;
        assert_eq!(
            from_slice::<Value>(input).unwrap(),
            serde_json::from_slice::<Value>(input).unwrap()
        );
    }
    #[test]
    fn trailing_and_nonfinite_data_are_refused() {
        for input in ["{} {}", "NaN", "1e999", "[1,]"] {
            assert!(from_slice::<Value>(input.as_bytes()).is_err());
        }
    }
}
