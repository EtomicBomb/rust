use std::io::{Error, Result, ErrorKind};
use std::path::{PathBuf};
use std::fs;

/// Recursively copies the contents of the directory `src` to the directory `dst`.
/// Analogous to `cp -rf src/* dst`.
pub(crate) fn copy_dir_all<S: Into<PathBuf>, D: Into<PathBuf>>(src: S, dst: D) -> Result<()> {
    copy_dir_mono(src.into(), dst.into())
}

/// Monomorphized version of `copy_dir`
fn copy_dir_mono(src: PathBuf, dst: PathBuf) -> Result<()> {
    let mut dirs: Vec<(PathBuf, PathBuf)> = Vec::default();

    if !src.is_dir() {
        return Err(Error::new(ErrorKind::Other, format!("src path `{src:?}` should be a directory")));
    }
    if !dst.is_dir() {
        return Err(Error::new(ErrorKind::Other, format!("dst path `{dst:?}` should be a directory")));
    }

    dirs.push((src, dst));

    while let Some((src, dst)) = dirs.pop() {
        match fs::create_dir(&dst) {
            Ok(()) => {},
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {},
            Err(e) => return Err(e),
        }

        for entry in src.read_dir()? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let src_filename = dbg!(entry.file_name());
            let src = src.join(&src_filename);
            let dst = dst.join(&src_filename);
            if file_type.is_dir() {
                dirs.push((src, dst));
            } else {
                fs::copy(&src, &dst)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::copy_dir_all;
    use tempfile::TempDir;
//     use std::fs::{write, read_to_string};
    use std::io::{Result};

    #[test]
    fn empty_dir() -> Result<()> {
        let src = TempDir::new()?;
        let dst = TempDir::new()?;
        let src = src.path();
        let dst = dst.path();
        copy_dir_all(src, dst)?;
        let mut dst = dst.read_dir()?;
        assert!(dst.next().is_none(), "we copied nothing into the destination");
        Ok(())
    }


}
