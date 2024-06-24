use std::fmt;
use std::marker::PhantomData;

use serde::{Serialize, Deserialize};

/// Append-only templates for lists of items.
///
/// Last line of the rendered output is a comment encoding the next insertion point.
#[derive(Debug, Clone)]
pub(crate) struct OffsetTemplate<F> {
    format: PhantomData<F>,
    contents: String,
    offset: Offset,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Offset {
    next_insert: usize,
    empty: bool,
}

impl<F> OffsetTemplate<F> {
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
    pub(crate) fn before_after(before: &str, after: &str) -> Self {
        let contents = format!("{before}{after}");
        let offset = Offset { next_insert: before.len(), empty: true };
        Self { format: PhantomData, contents, offset }
    }
}

impl<F: FileFormat> OffsetTemplate<F> {
    /// Puts the text `insert` at the template's insertion point
    pub(crate) fn append(&mut self, insert: &str) -> Result<(), Error> {
        if !self.contents.is_char_boundary(self.offset.next_insert) {
            return Err(Error);
        }
        let sep = if self.offset.empty { "" } else { F::SEPARATOR };
        self.offset.empty = false;
        let after = self.contents.split_off(self.offset.next_insert);
        self.contents.push_str(sep);
        self.contents.push_str(insert);
        self.contents.push_str(&after);
        self.offset.next_insert += sep.len() + insert.len();
        Ok(())
    }
}

impl<F: FileFormat> fmt::Display for OffsetTemplate<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let offset = serde_json::to_string(&self.offset).map_err(|_| fmt::Error)?;
        write!(f, "{}\n{}{}{}", self.contents, F::COMMENT_START, &offset, F::COMMENT_END)
    }
}

impl<F: FileFormat> TryFrom<String> for OffsetTemplate<F> {
    type Error = Error;
    fn try_from(file: String) -> Result<Self, Self::Error> {
        let newline_index = file.rfind('\n').ok_or(Error)?;
        let s = &file[newline_index+1..];
        let s = s.strip_prefix(F::COMMENT_START).ok_or(Error)?;
        let s = s.strip_suffix(F::COMMENT_END).ok_or(Error)?;
        let offset = serde_json::from_str(&s).map_err(|_| Error)?;
        let mut contents = file;
        contents.truncate(newline_index);
        Ok(OffsetTemplate { format: PhantomData, contents, offset })
    }
}

mod sealed {
    pub trait Sealed { }
}

pub(crate) trait FileFormat: sealed::Sealed {
    const COMMENT_START: &'static str;
    const COMMENT_END: &'static str;
    const SEPARATOR: &'static str;
}

#[derive(Debug, Clone)]
pub(crate) struct Html;

/// Suitable for HTML documents
impl sealed::Sealed for Html {}

impl FileFormat for Html {
    const COMMENT_START: &'static str = "<!--";
    const COMMENT_END: &'static str = "-->";
    const SEPARATOR: &'static str = "";
}

#[derive(Debug, Clone)]
pub(crate) struct Js;

/// Suitable for JS files with JSON arrays
impl sealed::Sealed for Js {}

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

    fn is_comment_js(s: &str) -> bool {
        s.starts_with("//")
    }

    fn is_comment_html(s: &str) -> bool {
        s.starts_with("<!--") && s.ends_with("-->")
    }

    #[test]
    fn html_from_empty() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>world</p>"];
        let mut template = OffsetTemplate::<Html>::before_after("", "");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, inserts.join(""));
        assert!(is_comment_html(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn html_page() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>world</p>"];
        let before = "<html><head></head><body>";
        let after = "</body>";
        let mut template = OffsetTemplate::<Html>::before_after(before, after);
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("{before}{}{after}", inserts.join("")));
        assert!(is_comment_html(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn js_from_empty() {
        let inserts = ["1", "2", "3"];
        let mut template = OffsetTemplate::<Js>::before_after("", "");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, inserts.join(","));
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn js_empty_array() {
        let template = OffsetTemplate::<Js>::before_after("[", "]");
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[]"));
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn js_number_array() {
        let inserts = ["1", "2", "3"];
        let mut template = OffsetTemplate::<Js>::before_after("[", "]");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[1,2,3]"));
        assert!(is_comment_js(end));
        assert!(!end.contains("\n"));
    }

    #[test]
    fn magic_js_number_array() {
        let inserts = ["1"];
        let mut template = OffsetTemplate::<Js>::magic("[#]", "#").unwrap();
        for insert in inserts {
            template.append(insert).unwrap();
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
        let mut template = OffsetTemplate::<Js>::before_after("[", "]");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template1 = format!("{template}");
        let mut template = OffsetTemplate::<Js>::try_from(template1.clone()).unwrap();
        assert_eq!(template1, format!("{template}"));
        template.append("4").unwrap();
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, "[1,2,3,4]");
        assert!(is_comment_js(end));
    }

    #[test]
    fn round_trip_html() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>world</p>"];
        let before = "<html><head></head><body>";
        let after = "</body>";
        let mut template = OffsetTemplate::<Html>::before_after(before, after);
        template.append(inserts[0]).unwrap();
        template.append(inserts[1]).unwrap();
        let template = format!("{template}");
        let mut template = OffsetTemplate::<Html>::try_from(template).unwrap();
        template.append(inserts[2]).unwrap();
        let template = format!("{template}");
        let (template, end) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("{before}{}{after}", inserts.join("")));
        assert!(is_comment_html(end));
    }
}
