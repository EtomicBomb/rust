#![allow(dead_code)]
use std::fmt;
use std::marker::PhantomData;

use serde::{Serialize, Deserialize};

#[derive(Debug, Clone)]
pub(crate) struct OffsetTemplate<F> {
    format: PhantomData<F>,
    contents: String,
    offset: Offset,
}

#[derive(Debug, Clone)]
#[derive(Serialize, Deserialize)]
struct Offset {
    byte_offset: usize,
    first: bool,
}

impl<F> OffsetTemplate<F> {
    pub(crate) fn magic(template: &str, magic: &str) -> Result<Self, Error> {
        let mut split = template.split(magic);
        let before = split.next().ok_or(Error)?;
        let after = split.next().ok_or(Error)?;
        if split.next().is_some() {
            return Err(Error);
        }
        Ok(Self::before_after(before, after))
    }

    pub(crate) fn before_after(before: &str, after: &str) -> Self {
        let contents = format!("{before}{after}");
        let offset = Offset { byte_offset: before.len(), first: true };
        Self { format: PhantomData, contents, offset }
    }
}

impl<F: FileFormat> OffsetTemplate<F> {
    pub(crate) fn append(&mut self, insert: &str) -> Result<(), Error> {
        if !self.contents.is_char_boundary(self.offset.byte_offset) {
            return Err(Error);
        }
        let sep = if self.offset.first { "" } else { F::SEPARATOR };
        self.offset.first = false;
        let after = self.contents.split_off(self.offset.byte_offset);
        self.contents.push_str(sep);
        self.contents.push_str(insert);
        self.contents.push_str(&after);
        self.offset.byte_offset += sep.len() + insert.len();
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

    #[test]
    fn html_from_empty() {
        let inserts = ["<p>hello</p>", "<p>kind</p>", "<p>world</p>"];
        let mut template = OffsetTemplate::<Html>::before_after("", "");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, _) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, inserts.join(""));
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
        let (template, _) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("{before}{}{after}", inserts.join("")));
    }

    #[test]
    fn js_from_empty() {
        let inserts = ["1", "2", "3"];
        let mut template = OffsetTemplate::<Js>::before_after("", "");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, _) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, inserts.join(","));
    }

    #[test]
    fn js_empty_array() {
        let template = OffsetTemplate::<Js>::before_after("[", "]");
        let template = format!("{template}");
        let (template, _) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[]"));
    }

    #[test]
    fn js_number_array() {
        let inserts = ["1", "2", "3"];
        let mut template = OffsetTemplate::<Js>::before_after("[", "]");
        for insert in inserts {
            template.append(insert).unwrap();
        }
        let template = format!("{template}");
        let (template, _) = template.rsplit_once("\n").unwrap();
        assert_eq!(template, format!("[1,2,3]"));
    }
}
