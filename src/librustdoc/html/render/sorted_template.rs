use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Append-only templates for sorted, deduplicated lists of items.
///
/// Last line of the rendered output is a comment encoding the next insertion point.
#[derive(Debug, Clone)]
pub(crate) struct SortedTemplate<F> {
    format: PhantomData<F>,
    before: String,
    after: String,
    contents: BTreeSet<String>,
}

/// Written to last line of file to specify the location of each fragment
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Offset {
    /// Index of the first byte in the template
    start: usize,
    /// The length of each fragment in the encoded template, including the separator
    delta: Vec<usize>,
}

impl<F> SortedTemplate<F> {
    /// Generate this template from arbitary text.
    /// Will insert wherever the substring `magic` can be found.
    /// Errors if it does not appear exactly once.
    pub(crate) fn magic(template: &str, magic: &str) -> Result<Self, Error> {
        let mut split = template.split(magic);
        let before = split.next().ok_or(Error)?;
        let after = split.next().ok_or(Error)?;
        if split.next().is_some() {
            return Err(Error);
        }
        Ok(Self::before_after(before, after))
    }

    /// Template will insert contents between `before` and `after`
    pub(crate) fn before_after<S: ToString, T: ToString>(before: S, after: T) -> Self {
        let before = before.to_string();
        let after = after.to_string();
        SortedTemplate { format: PhantomData, before, after, contents: Default::default() }
    }
}

impl<F: FileFormat> SortedTemplate<F> {
    /// Adds this text to the template
    pub(crate) fn append(&mut self, insert: String) {
        self.contents.insert(insert);
    }
}

impl<F: FileFormat> fmt::Display for SortedTemplate<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut delta = Vec::default();
        write!(f, "{}", self.before)?;
        let contents: Vec<_> = self.contents.iter().collect();
        let mut sep = "";
        for content in contents {
            delta.push(sep.len() + content.len());
            write!(f, "{}{}", sep, content)?;
            sep = F::SEPARATOR;
        }
        let offset = Offset { start: self.before.len(), delta };
        let offset = serde_json::to_string(&offset).unwrap();
        write!(f, "{}\n{}{}{}", self.after, F::COMMENT_START, offset, F::COMMENT_END)?;
        Ok(())
    }
}

fn checked_split_at(s: &str, index: usize) -> Option<(&str, &str)> {
    s.is_char_boundary(index).then(|| s.split_at(index))
}

impl<F: FileFormat> FromStr for SortedTemplate<F> {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (s, offset) = s.rsplit_once("\n").ok_or(Error)?;
        let offset = offset.strip_prefix(F::COMMENT_START).ok_or(Error)?;
        let offset = offset.strip_suffix(F::COMMENT_END).ok_or(Error)?;
        let offset: Offset = serde_json::from_str(&offset).map_err(|_| Error)?;
        let (before, mut s) = checked_split_at(s, offset.start).ok_or(Error)?;
        let mut contents = BTreeSet::default();
        let mut sep = "";
        for &index in offset.delta.iter() {
            let (content, rest) = checked_split_at(s, index).ok_or(Error)?;
            s = rest;
            let content = content.strip_prefix(sep).ok_or(Error)?;
            contents.insert(content.to_string());
            sep = F::SEPARATOR;
        }
        Ok(SortedTemplate {
            format: PhantomData,
            before: before.to_string(),
            after: s.to_string(),
            contents,
        })
    }
}

pub(crate) trait FileFormat {
    const COMMENT_START: &'static str;
    const COMMENT_END: &'static str;
    const SEPARATOR: &'static str;
}

#[derive(Debug, Clone)]
pub(crate) struct Html;

impl FileFormat for Html {
    const COMMENT_START: &'static str = "<!--";
    const COMMENT_END: &'static str = "-->";
    const SEPARATOR: &'static str = "";
}

#[derive(Debug, Clone)]
pub(crate) struct Js;

impl FileFormat for Js {
    const COMMENT_START: &'static str = "//";
    const COMMENT_END: &'static str = "";
    const SEPARATOR: &'static str = ",";
}

#[derive(Debug, Clone)]
pub(crate) struct Error;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid template")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn is_comment_js(s: &str) -> bool {
        s.starts_with("//")
    }

    fn is_comment_html(s: &str) -> bool {
        // not correct but good enough for these tests
        s.starts_with("<!--") && s.ends_with("-->")
    }

    #[test]
    fn html_from_empty() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>hello</p>", "<p>world</p>"];
        let mut template = SortedTemplate::<Html>::before_after("", "");
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, "<p>hello</p><p>kind</p><p>world</p>");
        assert!(is_comment_html(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn html_page() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>world</p>"];
        let before = "<html><head></head><body>";
        let after = "</body>";
        let mut template = SortedTemplate::<Html>::before_after(before, after);
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("{before}{}{after}", inserts.join("")));
        assert!(is_comment_html(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn js_from_empty() {
        let inserts = ["1", "2", "2", "2", "3", "1"];
        let mut template = SortedTemplate::<Js>::before_after("", "");
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, "1,2,3");
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn js_empty_array() {
        let template = SortedTemplate::<Js>::before_after("[", "]");
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[]"));
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn js_number_array() {
        let inserts = ["1", "2", "3"];
        let mut template = SortedTemplate::<Js>::before_after("[", "]");
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[1,2,3]"));
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn magic_js_number_array() {
        let inserts = ["1", "1"];
        let mut template = SortedTemplate::<Js>::magic("[#]", "#").unwrap();
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[1]"));
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn round_trip_js() {
        let inserts = ["1", "2", "3"];
        let mut template = SortedTemplate::<Js>::before_after("[", "]");
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template1 = format!("{template}");
        let mut template = SortedTemplate::<Js>::from_str(&template1).unwrap();
        assert_eq!(template1, format!("{template}"));
        template.append("4".to_string());
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, "[1,2,3,4]");
        assert!(is_comment_js(end));
    }

    #[test]
    fn round_trip_html() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>world</p>", "<p>kind</p>"];
        let before = "<html><head></head><body>";
        let after = "</body>";
        let mut template = SortedTemplate::<Html>::before_after(before, after);
        template.append(inserts[0].to_string());
        template.append(inserts[1].to_string());
        let template = format!("{template}");
        let mut template = SortedTemplate::<Html>::from_str(&template).unwrap();
        template.append(inserts[2].to_string());
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("{before}<p>hello</p><p>kind</p><p>world</p>{after}"));
        assert!(is_comment_html(end));
    }

    #[test]
    fn blank_js() {
        let inserts = ["1", "2", "3"];
        let template = SortedTemplate::<Js>::before_after("", "");
        let template = format!("{template}");
        let (t, _) = template.rsplit_once("\n").unwrap();
        assert_eq!(t, "");
        let mut template = SortedTemplate::<Js>::from_str(&template).unwrap();
        for insert in inserts {
            template.append(insert.to_string());
        }
        let template1 = format!("{template}");
        let mut template = SortedTemplate::<Js>::from_str(&template1).unwrap();
        assert_eq!(template1, format!("{template}"));
        template.append("4".to_string());
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, "1,2,3,4");
        assert!(is_comment_js(end));
    }
}
