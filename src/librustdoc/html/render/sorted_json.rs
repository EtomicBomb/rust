use serde_json::Value;
use serde::{Serialize, Deserialize};
use std::fmt;
use std::borrow::Borrow;
use itertools::Itertools as _;

/// Prerenedered json.
///
/// Arrays are sorted by their stringified entries, and objects are sorted by their stringified
/// keys.
///
/// Must use serde_json with the preserve_order feature.
///
/// Both the Display and serde_json::to_string implementations write the serialized json
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "Value")]
#[serde(into = "Value")]
pub struct SortedJson(String);

impl SortedJson {
    /// If you pass in an array, it will not be sorted.
    pub fn serialize<T: Serialize>(item: T) -> Self {
        SortedJson(serde_json::to_string(&item).unwrap())
    }

    /// Assumes that `item` is already JSON encoded
    ///
    /// TODO: remove this, and use SortedJson everywhere JSON is rendered
    pub fn preserialized(item: String) -> Self {
        SortedJson(item)
    }

    /// Serializes and sorts
    pub fn array<T: Borrow<SortedJson>, I: IntoIterator<Item=T>>(items: I) -> Self {
        let items = items.into_iter()
            .sorted_unstable_by(|a, b| a.borrow().cmp(&b.borrow()))
            .format_with(",", |item, f| f(item.borrow()));
        SortedJson(format!("[{}]", items))
    }

    pub fn array_unsorted<T: Borrow<SortedJson>, I: IntoIterator<Item=T>>(items: I) -> Self {
        let items = items.into_iter().format_with(",", |item, f| f(item.borrow()));
        SortedJson(format!("[{items}]"))
    }

    pub fn object<K, V, I>(items: I) -> Self
        where K: Borrow<SortedJson>,
              V: Borrow<SortedJson>,
              I: IntoIterator<Item=(K, V)>,
    {
        let items = items.into_iter()
            .sorted_unstable_by(|a, b| a.0.borrow().cmp(&b.0.borrow()))
            .format_with(",", |(k, v), f| f(&format_args!("{}:{}", k.borrow(), v.borrow())));
        SortedJson(format!("{{{}}}", items))
    }
}

impl fmt::Display for SortedJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Value> for SortedJson {
    fn from(value: Value) -> Self {
        SortedJson(serde_json::to_string(&value).unwrap())
    }
}

impl From<SortedJson> for Value {
    fn from(json: SortedJson) -> Self {
        serde_json::from_str(&json.0).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(json: SortedJson, serialized: &str) {
        assert_eq!(json.to_string(), serialized);
        assert_eq!(serde_json::to_string(&json).unwrap(), serialized);
        let json = json.to_string();
        let json: SortedJson = serde_json::from_str(&json).unwrap();
        assert_eq!(json.to_string(), serialized);
        assert_eq!(serde_json::to_string(&json).unwrap(), serialized);
    }

    #[test]
    fn number() {
        let json = SortedJson::serialize(3);
        let serialized = "3";
        check(json, serialized);
    }

    #[test]
    fn boolean() {
        let json = SortedJson::serialize(true);
        let serialized = "true";
        check(json, serialized);
    }

    #[test]
    fn serialize_array() {
        let json = SortedJson::serialize([3, 1, 2]);
        let serialized =  "[3,1,2]";
        check(json, serialized);
    }

    #[test]
    fn sorted_array() {
        let items = ["c", "a", "b"];
        let serialized = r#"["a","b","c"]"#;
        let items: Vec<SortedJson> = items.into_iter().map(SortedJson::serialize).collect();
        let json = SortedJson::array(items);
        check(json, serialized);
    }

    #[test]
    fn array_unsorted() {
        let items = ["c", "a", "b"];
        let serialized = r#"["c","a","b"]"#;
        let items: Vec<SortedJson> = items.into_iter().map(SortedJson::serialize).collect();
        let json = SortedJson::array_unsorted(items);
        check(json, serialized);
    }

    #[test]
    fn object() {
        let items = [("c", 1), ("a", 10), ("b", 3)];
        let serialized = r#"{"a":10,"b":3,"c":1}"#;
        let items: Vec<(SortedJson, SortedJson)> = items.into_iter()
            .map(|(k, v)| (SortedJson::serialize(k), SortedJson::serialize(v)))
            .collect();
        let json = SortedJson::object(items);
        check(json, serialized);
    }
}
