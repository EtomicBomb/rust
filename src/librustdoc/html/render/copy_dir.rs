use std::fs;
use std::io::{Error, ErrorKind, Result};
use std::path::PathBuf;

/// Recursively copies the contents of the directory `src` to the directory `dst`.
/// Analogous to `cp -rf src/* dst` and Python's `shutil.copytree`
///
/// Creates all directories needed to perform the copy.
///
/// Will overwrite files in the output directory.
pub(crate) fn copy_dir_all<S: Into<PathBuf>, D: Into<PathBuf>>(src: S, dst: D) -> Result<()> {
    copy_dir_all_mono(src.into(), dst.into())
}

/// Helper for `copy_dir`
fn copy_dir_all_mono(src: PathBuf, dst: PathBuf) -> Result<()> {
    let mut dirs: Vec<(PathBuf, PathBuf)> = Vec::default();

    if !src.is_dir() {
        return Err(Error::new(
            ErrorKind::Other,
            format!("src path `{src:?}` should be a directory"),
        ));
    }
    if !dst.is_dir() {
        return Err(Error::new(
            ErrorKind::Other,
            format!("dst path `{dst:?}` should be a directory"),
        ));
    }

    dirs.push((src, dst));

    while let Some((src, dst)) = dirs.pop() {
        match fs::create_dir(&dst) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }

        for entry in src.read_dir()? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let src_filename = entry.file_name();
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
    use std::fs;
    use std::io::Result;
    use std::path::Path;
    use tempfile::TempDir;

    fn create_paths(root: &Path, paths: &[&str]) -> Result<()> {
        for path in paths {
            let is_directory = path.ends_with("/");
            let path = root.join(path);
            if is_directory {
                fs::create_dir_all(&path)?;
            } else {
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(&path, "")?;
            }
        }

        Ok(())
    }

    fn verify_paths(root: &Path, paths: &[&str]) {
        for path in paths {
            let should_create_directory = path.ends_with("/");
            let path = root.join(path);
            if should_create_directory {
                assert!(path.is_dir(), "expected {path:?} to be directory");
            } else {
                assert!(path.is_file(), "expected {path:?} to be a file");
            }
        }
    }

    fn run_test(paths: &[&str]) -> Result<()> {
        let src = TempDir::with_prefix("src")?;
        let src = src.path();
        let dst = TempDir::with_prefix("dst")?;
        let dst = dst.path();
        create_paths(src, paths)?;
        verify_paths(src, paths);
        copy_dir_all(src, dst)?;
        verify_paths(src, paths);
        verify_paths(dst, paths);
        Ok(())
    }

    #[test]
    fn empty_dir() -> Result<()> {
        run_test(&[])
    }

    #[test]
    fn one_file() -> Result<()> {
        run_test(&["a"])
    }

    #[test]
    fn directory_no_files() -> Result<()> {
        run_test(&["a/"])
    }

    #[test]
    fn one_file_directory() -> Result<()> {
        run_test(&["a", "b/c"])
    }

    #[test]
    fn nested_directory() -> Result<()> {
        run_test(&["b/c/d/e/f"])
    }

    #[test]
    fn two_directory() -> Result<()> {
        run_test(&["a/a", "b/b"])
    }

    #[test]
    fn two_directory2() -> Result<()> {
        run_test(&["a/b", "b/a"])
    }

    #[test]
    fn directory_with_multiple_files() -> Result<()> {
        run_test(&["a/a", "a/b", "a/c"])
    }

    #[test]
    fn multiple_directories_with_multiple_files() -> Result<()> {
        run_test(&["d/", "a/a", "a/b/", "a/c", "a/b/c/d", "b"])
    }
}
