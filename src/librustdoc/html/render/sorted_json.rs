use itertools::Itertools as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Borrow;
use std::fmt;

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
pub(crate) struct SortedJson(String);

impl SortedJson {
    /// If you pass in an array, it will not be sorted.
    pub(crate) fn serialize<T: Serialize>(item: T) -> Self {
        SortedJson(serde_json::to_string(&item).unwrap())
    }

    /// Serializes and sorts
    pub(crate) fn array<T: Borrow<SortedJson>, I: IntoIterator<Item = T>>(items: I) -> Self {
        let items = items
            .into_iter()
            .sorted_unstable_by(|a, b| a.borrow().cmp(&b.borrow()))
            .format_with(",", |item, f| f(item.borrow()));
        SortedJson(format!("[{}]", items))
    }

    pub(crate) fn array_unsorted<T: Borrow<SortedJson>, I: IntoIterator<Item = T>>(
        items: I,
    ) -> Self {
        let items = items.into_iter().format_with(",", |item, f| f(item.borrow()));
        SortedJson(format!("[{items}]"))
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

/// For use in JSON.parse('{...}').
///
/// JSON.parse supposedly loads faster than raw JS source,
/// so this is used for large objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EscapedJson(SortedJson);

impl From<SortedJson> for EscapedJson {
    fn from(json: SortedJson) -> Self {
        EscapedJson(json)
    }
}

impl fmt::Display for EscapedJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // All these `replace` calls are because we have to go through JS string
        // for JSON content.
        // We need to escape double quotes for the JSON
        let json = self.0.0.replace('\\', r"\\").replace('\'', r"\'").replace("\\\"", "\\\\\"");
        write!(f, "{}", json)
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

        let json = serde_json::to_string(&json).unwrap();
        let json: SortedJson = serde_json::from_str(&json).unwrap();

        assert_eq!(json.to_string(), serialized);
        assert_eq!(serde_json::to_string(&json).unwrap(), serialized);
    }

    #[test]
    fn escape_json_number() {
        let json = SortedJson::serialize(3);
        let json = EscapedJson::from(json);
        assert_eq!(format!("{json}"), "3");
    }

    #[test]
    fn escape_json_single_quote() {
        let json = SortedJson::serialize("he's");
        let json = EscapedJson::from(json);
        assert_eq!(dbg!(format!("{json}")), r#""he\'s""#);
    }

    #[test]
    fn escape_json_array() {
        let json = SortedJson::serialize([1,2,3]);
        let json = EscapedJson::from(json);
        assert_eq!(dbg!(format!("{json}")), r#"[1,2,3]"#);
    }

    #[test]
    fn escape_json_string() {
        let json = SortedJson::serialize(r#"he"llo"#);
        let json = EscapedJson::from(json);
        assert_eq!(dbg!(format!("{json}")), r#""he\\\"llo""#);
    }

    #[test]
    fn escape_json_string_escaped() {
        let json = SortedJson::serialize(r#"he\"llo"#);
        let json = EscapedJson::from(json);
        assert_eq!(format!("{json}"), r#""he\\\\\\\"llo""#);
    }

    #[test]
    fn escape_json_string_escaped_escaped() {
        let json = SortedJson::serialize(r#"he\\"llo"#);
        let json = EscapedJson::from(json);
        assert_eq!(format!("{json}"), r#""he\\\\\\\\\\\"llo""#);
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
    fn string() {
        let json = SortedJson::serialize("he\"llo");
        let serialized = r#""he\"llo""#;
        check(json, serialized);
    }

    #[test]
    fn serialize_array() {
        let json = SortedJson::serialize([3, 1, 2]);
        let serialized = "[3,1,2]";
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
    fn nested_array() {
        let a = SortedJson::serialize(3);
        let b = SortedJson::serialize(2);
        let c = SortedJson::serialize(1);
        let d = SortedJson::serialize([1, 3, 2]);
        let json = SortedJson::array([a, b, c, d]);
        let serialized = r#"[1,2,3,[1,3,2]]"#;
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
}
