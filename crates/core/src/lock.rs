//! `lenslet.lock`: the AWS access a project's runs are known to need.
//!
//! One permission per line, `action  resource`, sorted, so a pull request
//! that widens access shows up as an added line in review. Comments start
//! with `#`. The file is written by `lenslet run --update` and by
//! [`accept`] / [`set_test_mark`] / [`remove`] (the window), always
//! atomically and always in the canonical [`render`] format.
//!
//! What a rewrite keeps: every `# test` mark, and the full-line comments at
//! the top of the file (they replace the generated header). Other comments,
//! and blank lines, are not kept: the entries are re-rendered.

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::access::Access;

pub const FILE_NAME: &str = "lenslet.lock";

/// The header a new lock file gets.
const DEFAULT_HEADER: &str = "# AWS access observed by `lenslet run`: action, resource.\n\
     # Regenerate with `lenslet run --update -- <tests>`; review changes like code.\n\
     # Mark a line `# test` when only the tests need it; `lenslet policy` leaves it out.\n";

/// `lenslet.lock` in `dir`.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// A lock file: the access it lists, and which of it is test-only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lock {
    pub access: BTreeSet<Access>,
    /// Lines marked `# test`: needed by the tests themselves (fixtures that
    /// create tables or buckets), kept for `--check`, left out of `policy`.
    pub test: BTreeSet<Access>,
    /// The full-line comments before the first entry, kept on rewrite.
    pub header: Vec<String>,
}

/// The lock, or `None` when there is no lock file.
pub fn read(path: &Path) -> Result<Option<Lock>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    Ok(Some(parse(&text)))
}

pub fn parse(text: &str) -> Lock {
    let mut lock = Lock::default();
    let mut in_header = true;
    for line in text.lines() {
        let (entry, comment) = line.split_once('#').unwrap_or((line, ""));
        let mut fields = entry.split_whitespace();
        let Some(action) = fields.next() else {
            if in_header && line.trim_start().starts_with('#') {
                lock.header.push(line.trim_end().to_string());
            }
            continue;
        };
        in_header = false;
        let access = Access {
            action: action.to_string(),
            resource: fields.next().unwrap_or("*").to_string(),
        };
        if comment
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| word == "test")
        {
            lock.test.insert(access.clone());
        }
        lock.access.insert(access);
    }
    lock
}

/// The text of a lock listing `access`, with `test` marks, under `header`
/// comments (the default header when there are none).
pub fn render<'a>(
    header: &[String],
    access: impl IntoIterator<Item = &'a Access>,
    test: &BTreeSet<Access>,
) -> String {
    let entries: Vec<&Access> = access.into_iter().collect();
    let width = entries
        .iter()
        .map(|access| access.action.len())
        .max()
        .unwrap_or(0);
    let mut out = if header.is_empty() {
        DEFAULT_HEADER.to_string()
    } else {
        header.iter().map(|line| format!("{line}\n")).collect()
    };
    for access in entries {
        out.push_str(&format!("{:<width$}  {}", access.action, access.resource));
        if test.contains(access) {
            out.push_str("  # test");
        }
        out.push('\n');
    }
    out
}

fn render_lock(lock: &Lock) -> String {
    render(&lock.header, &lock.access, &lock.test)
}

/// Write the lock from a run's access (`lenslet run --update`), keeping the
/// file's header comments and the `test` marks given.
pub fn write<'a>(
    path: &Path,
    access: impl IntoIterator<Item = &'a Access>,
    test: &BTreeSet<Access>,
) -> Result<()> {
    let header = read(path)?.map(|lock| lock.header).unwrap_or_default();
    write_atomic(path, &render(&header, access, test))
}

/// Add permissions to the lock (creating it if needed), each with its `test`
/// mark. Lines already in the lock are left as they are. Returns how many
/// lines were added; the file is only rewritten when that is not zero.
pub fn accept(path: &Path, additions: &[(Access, bool)]) -> Result<usize> {
    let mut lock = read(path)?.unwrap_or_default();
    let mut added = 0;
    for (access, test) in additions {
        if lock.access.insert(access.clone()) {
            added += 1;
            if *test {
                lock.test.insert(access.clone());
            }
        }
    }
    if added > 0 {
        write_atomic(path, &render_lock(&lock))?;
    }
    Ok(added)
}

/// Mark (or unmark) a lock line `# test`. Returns whether the file changed:
/// `false` when the line is not in the lock or already marked that way.
pub fn set_test_mark(path: &Path, access: &Access, test: bool) -> Result<bool> {
    let Some(mut lock) = read(path)? else {
        return Ok(false);
    };
    if !lock.access.contains(access) || lock.test.contains(access) == test {
        return Ok(false);
    }
    if test {
        lock.test.insert(access.clone());
    } else {
        lock.test.remove(access);
    }
    write_atomic(path, &render_lock(&lock))?;
    Ok(true)
}

/// Remove permissions from the lock (with their `# test` marks). Returns how
/// many entries were removed.
pub fn remove(path: &Path, removals: &[Access]) -> Result<usize> {
    let Some(mut lock) = read(path)? else {
        return Ok(0);
    };
    let mut removed = 0usize;
    for access in removals {
        if lock.access.remove(access) {
            lock.test.remove(access);
            removed += 1;
        }
    }
    if removed > 0 {
        write_atomic(path, &render_lock(&lock))?;
    }
    Ok(removed)
}

/// Replace `path` with `text` atomically: a temporary file in the same
/// directory, synced, given the old file's permissions, then renamed over it.
/// A symlinked lock is written through to its target.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| FILE_NAME.to_string());
    // Unique per process *and* per write: two threads of one process (the
    // window and a run) never share a temporary file.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()));
    let permissions = fs::metadata(&target).ok().map(|meta| meta.permissions());
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temp, permissions)?;
        }
        fs::rename(&temp, &target)?;
        // Make the rename itself durable (best effort; not every platform
        // can open a directory).
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(action: &str, resource: &str) -> Access {
        Access::new(action, resource)
    }

    #[test]
    fn round_trips_through_a_reviewable_text_file() {
        let used: BTreeSet<Access> = [
            access("sqs:SendMessage", "arn:aws:sqs:*:*:jobs"),
            access("dynamodb:CreateTable", "arn:aws:dynamodb:*:*:table/orders"),
            access("kms:Decrypt", "arn:aws:kms:*:*:key/*"),
        ]
        .into();
        let test: BTreeSet<Access> = [access(
            "dynamodb:CreateTable",
            "arn:aws:dynamodb:*:*:table/orders",
        )]
        .into();
        let text = render(&[], &used, &test);
        assert!(text.starts_with(DEFAULT_HEADER), "{text}");
        assert!(
            text.contains("kms:Decrypt           arn:aws:kms:*:*:key/*\n"),
            "{text}"
        );
        assert!(
            text.contains("dynamodb:CreateTable  arn:aws:dynamodb:*:*:table/orders  # test\n"),
            "{text}"
        );
        let parsed = parse(&text);
        assert_eq!(parsed.access, used);
        assert_eq!(parsed.test, test, "test marks survive a rewrite");
        assert_eq!(
            parse("s3:ListAllMyBuckets\n# note\n\n").access,
            [access("s3:ListAllMyBuckets", "*")].into()
        );
        assert!(
            parse("s3:GetObject arn  # tested elsewhere\n")
                .test
                .is_empty(),
            "whole word only"
        );
        assert!(
            read(&path_in(Path::new("/nonexistent-lenslet")))
                .unwrap()
                .is_none()
        );
    }

    fn temp_lock(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lenslet-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn accept_marks_and_remove_rewrite_atomically_in_the_canonical_format() {
        let path = temp_lock("accept");
        let orders = access("dynamodb:PutItem", "arn:aws:dynamodb:*:*:table/orders");
        assert_eq!(accept(&path, &[(orders.clone(), false)]).unwrap(), 1);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(DEFAULT_HEADER), "{text}");
        assert!(text.ends_with("dynamodb:PutItem  arn:aws:dynamodb:*:*:table/orders\n"));

        // The user's own header comments and `test` marks survive; other
        // comments are not kept.
        fs::write(
            &path,
            "# Owned by the payments team.\n\
             # Review with #infra.\n\
             \n\
             kms:Decrypt  arn:aws:kms:*:*:key/*  # needed by S3\n\
             # stray note\n\
             dynamodb:PutItem  arn:aws:dynamodb:*:*:table/orders  # why: checkout\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let table = access("dynamodb:CreateTable", "arn:aws:dynamodb:*:*:table/orders");
        assert_eq!(
            accept(&path, &[(table.clone(), true), (orders.clone(), true)]).unwrap(),
            1,
            "a line already in the lock is left as it is"
        );
        let expected = "# Owned by the payments team.\n\
             # Review with #infra.\n\
             dynamodb:CreateTable  arn:aws:dynamodb:*:*:table/orders  # test\n\
             dynamodb:PutItem      arn:aws:dynamodb:*:*:table/orders\n\
             kms:Decrypt           arn:aws:kms:*:*:key/*\n";
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640, "the rewrite keeps the file's permissions");
        }
        assert_eq!(accept(&path, &[(orders.clone(), false)]).unwrap(), 0);

        assert!(set_test_mark(&path, &orders, true).unwrap());
        assert!(
            !set_test_mark(&path, &orders, true).unwrap(),
            "already marked"
        );
        assert!(set_test_mark(&path, &table, false).unwrap());
        assert!(!set_test_mark(&path, &access("s3:GetObject", "*"), true).unwrap());
        let lock = read(&path).unwrap().unwrap();
        assert_eq!(lock.test, [orders.clone()].into());
        assert_eq!(lock.header.len(), 2);

        assert_eq!(remove(&path, std::slice::from_ref(&table)).unwrap(), 1);
        assert_eq!(remove(&path, std::slice::from_ref(&table)).unwrap(), 0);

        // `--update` rewrites the entries but keeps the header.
        write(&path, [&orders], &lock.test).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Owned by the payments team.\n\
             # Review with #infra.\n\
             dynamodb:PutItem  arn:aws:dynamodb:*:*:table/orders  # test\n"
        );
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "no temporary files are left behind");
        assert!(!set_test_mark(&temp_lock("missing"), &orders, true).unwrap());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn concurrent_writes_from_one_process_never_share_a_temporary_file() {
        let path = temp_lock("threads");
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..10 {
                        let entry = access(
                            "sqs:SendMessage",
                            &format!("arn:aws:sqs:*:*:q{index}-{round}"),
                        );
                        accept(&path, &[(entry, false)]).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let lock = read(&path).unwrap().unwrap();
        assert!(!lock.access.is_empty());
        let dir = path.parent().unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(dir);
    }
}
