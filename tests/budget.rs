extern crate tar;
extern crate tempfile;

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Cursor, Seek, Write};
use std::process::Command;

use tar::{
    Archive, BudgetErrorKind, EntryType, Header, UnpackBudget, UnpackBudgetError,
    DEFAULT_MAX_ENTRIES, DEFAULT_MAX_FILE_FRACTION, DEFAULT_MAX_SPARSE_RANGES,
    DEFAULT_MAX_TOTAL_BYTES,
};
use tempfile::Builder as TempBuilder;

fn t<T>(r: io::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{}", e),
    }
}

fn drain<R: io::Read>(ar: &mut Archive<R>) -> io::Result<()> {
    for entry in t(ar.entries()) {
        entry?;
    }
    Ok(())
}

fn budget_error_kind(err: &io::Error) -> BudgetErrorKind {
    // `io::Error::get_ref` exposes the boxed cause regardless of how the
    // standard library forwards `Error::source`.
    let budget = err
        .get_ref()
        .and_then(|s| s.downcast_ref::<UnpackBudgetError>())
        .expect("error should carry an UnpackBudgetError");
    budget.kind()
}

fn find_budget_error(err: &(dyn Error + 'static)) -> Option<UnpackBudgetError> {
    if let Some(budget) = err.downcast_ref::<UnpackBudgetError>() {
        return Some(*budget);
    }
    if let Some(io_err) = err.downcast_ref::<io::Error>() {
        if let Some(inner) = io_err.get_ref() {
            if let Some(found) = find_budget_error(inner) {
                return Some(found);
            }
        }
    }
    let mut source = err.source();
    while let Some(inner) = source {
        if let Some(found) = find_budget_error(inner) {
            return Some(found);
        }
        source = inner.source();
    }
    None
}

/// Builds an in-memory tar containing `count` regular files of `entry_size`
/// bytes each.
fn archive_of_files(count: usize, entry_size: u64) -> Vec<u8> {
    let mut ar = tar::Builder::new(Vec::new());
    let data = vec![0u8; entry_size as usize];
    for i in 0..count {
        let mut header = Header::new_gnu();
        t(header.set_path(format!("file-{}", i)));
        header.set_size(entry_size);
        header.set_entry_type(EntryType::file());
        header.set_cksum();
        t(ar.append(&header, Cursor::new(&data[..])));
    }
    t(ar.into_inner())
}

#[test]
fn default_budget_constants_are_permissive() {
    let budget = UnpackBudget::default();
    assert_eq!(budget.max_entries(), DEFAULT_MAX_ENTRIES);
    assert_eq!(budget.max_total_bytes(), DEFAULT_MAX_TOTAL_BYTES);
    assert_eq!(budget.max_sparse_ranges(), DEFAULT_MAX_SPARSE_RANGES);
    assert_eq!(budget.max_file_fraction(), DEFAULT_MAX_FILE_FRACTION);

    // The per-file cap derives from the total budget and fraction.
    assert!(budget.max_file_fraction() > 0.0 && budget.max_file_fraction() <= 1.0);
}

#[test]
fn unlimited_budget_disables_limits() {
    let budget = UnpackBudget::unlimited();
    assert_eq!(budget.max_entries(), u64::MAX);
    assert_eq!(budget.max_total_bytes(), u64::MAX);
    assert_eq!(budget.max_sparse_ranges(), u64::MAX);
    assert_eq!(budget.max_file_fraction(), 1.0);
}

#[test]
fn builder_setters_chain_and_validate_fraction() {
    let mut budget = UnpackBudget::default();
    budget
        .set_max_entries(7)
        .set_max_total_bytes(1024)
        .set_max_sparse_ranges(3);
    t(budget.set_max_file_fraction(0.5));

    assert_eq!(budget.max_entries(), 7);
    assert_eq!(budget.max_total_bytes(), 1024);
    assert_eq!(budget.max_sparse_ranges(), 3);
    assert_eq!(budget.max_file_fraction(), 0.5);
    assert_eq!(
        (budget.max_total_bytes() as f64 * budget.max_file_fraction()) as u64,
        512
    );

    assert!(budget.set_max_file_fraction(-0.1).is_err());
    assert!(budget.set_max_file_fraction(1.1).is_err());
    // Budget left untouched after a rejected value.
    assert_eq!(budget.max_file_fraction(), 0.5);
}

#[test]
fn archive_exposes_configured_budget() {
    let mut ar = Archive::new(Cursor::new(Vec::new()));
    assert_eq!(ar.unpack_budget().max_entries(), DEFAULT_MAX_ENTRIES);

    ar.set_unpack_budget(UnpackBudget::unlimited());
    assert_eq!(ar.unpack_budget().max_entries(), u64::MAX);
}

#[test]
fn too_many_entries_is_rejected_while_iterating() {
    let bytes = archive_of_files(5, 1);
    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(4, u64::MAX, 1.0, u64::MAX)));

    let err = drain(&mut ar).expect_err("entry limit must be enforced");
    assert_eq!(budget_error_kind(&err), BudgetErrorKind::EntryLimitExceeded);
}

#[test]
fn budget_is_enforced_for_raw_and_seekable_iteration() {
    let bytes = archive_of_files(3, 1);

    // Raw iteration counts each physical header against the budget.
    let mut raw = Archive::new(Cursor::new(bytes.clone()));
    raw.set_unpack_budget(t(UnpackBudget::with_limits(2, u64::MAX, 1.0, u64::MAX)));
    let err = t(raw.entries())
        .raw(true)
        .map(|e| e.map(|_| ()))
        .collect::<io::Result<Vec<_>>>()
        .expect_err("raw iteration must enforce the budget");
    assert_eq!(budget_error_kind(&err), BudgetErrorKind::EntryLimitExceeded);

    // Seekable iteration enforces the same budget.
    let mut seekable = Archive::new(Cursor::new(bytes));
    seekable.set_unpack_budget(t(UnpackBudget::with_limits(2, u64::MAX, 1.0, u64::MAX)));
    let err = t(seekable.entries_with_seek())
        .map(|e| e.map(|_| ()))
        .collect::<io::Result<Vec<_>>>()
        .expect_err("seekable iteration must enforce the budget");
    assert_eq!(budget_error_kind(&err), BudgetErrorKind::EntryLimitExceeded);
}

#[test]
fn too_many_entries_is_rejected_when_unpacking() {
    let bytes = archive_of_files(5, 1);
    let td = t(TempBuilder::new().prefix("tar-budget").tempdir());
    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(4, u64::MAX, 1.0, u64::MAX)));

    let err = ar.unpack(td.path()).expect_err("unpack must fail");
    // The budget error is the root cause of the wrapped TarError.
    let found = find_budget_error(&err);
    assert!(
        found.is_some(),
        "unpack error should chain an UnpackBudgetError: {}",
        err
    );
    assert_eq!(found.unwrap().kind(), BudgetErrorKind::EntryLimitExceeded);
}

#[test]
fn total_byte_budget_is_enforced() {
    // Three files of 100 bytes each: 300 bytes total but a 250 byte budget.
    let bytes = archive_of_files(3, 100);
    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(u64::MAX, 250, 1.0, u64::MAX)));

    let err = drain(&mut ar).expect_err("total byte limit must be enforced");
    assert_eq!(
        budget_error_kind(&err),
        BudgetErrorKind::TotalBytesLimitExceeded
    );
}

#[test]
fn single_file_fraction_is_enforced() {
    // One file of 100 bytes, total budget 1000 but fraction 0.05 -> cap 50.
    let bytes = archive_of_files(1, 100);
    let mut ar = Archive::new(Cursor::new(bytes));
    let mut budget = t(UnpackBudget::with_limits(u64::MAX, 1000, 1.0, u64::MAX));
    t(budget.set_max_file_fraction(0.05));
    ar.set_unpack_budget(budget);

    let err = drain(&mut ar).expect_err("single file limit must be enforced");
    assert_eq!(
        budget_error_kind(&err),
        BudgetErrorKind::FileSizeLimitExceeded
    );
}

#[test]
fn file_at_the_limit_is_allowed() {
    let bytes = archive_of_files(2, 50);
    let mut ar = Archive::new(Cursor::new(bytes));
    let mut budget = t(UnpackBudget::with_limits(u64::MAX, 100, 1.0, u64::MAX));
    t(budget.set_max_file_fraction(0.5));
    ar.set_unpack_budget(budget);

    let mut count = 0;
    for entry in t(ar.entries()) {
        t(entry);
        count += 1;
    }
    assert_eq!(count, 2);
}

#[test]
fn unlimited_budget_lets_synthetic_archive_unpack() {
    let bytes = archive_of_files(20, 64);
    let td = t(TempBuilder::new().prefix("tar-budget").tempdir());
    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(UnpackBudget::unlimited());
    t(ar.unpack(td.path()));

    let mut found = 0;
    for entry in t(fs::read_dir(td.path())) {
        let entry = t(entry);
        let metadata = t(entry.metadata());
        assert_eq!(metadata.len(), 64);
        found += 1;
    }
    assert_eq!(found, 20);
}

#[test]
fn default_budget_allows_normal_archive() {
    let bytes = archive_of_files(10, 4096);
    let td = t(TempBuilder::new().prefix("tar-budget").tempdir());
    let mut ar = Archive::new(Cursor::new(bytes));
    t(ar.unpack(td.path()));

    let count = t(fs::read_dir(td.path())).count();
    assert_eq!(count, 10);
}

/// Builds a tar containing one GNU sparse entry with `ranges` data ranges
/// using the system `tar` binary, returning the raw archive bytes.
fn gnu_sparse_archive(ranges: usize) -> Vec<u8> {
    let work = t(TempBuilder::new().prefix("tar-sparse").tempdir());
    let path = work.path().join("sparse.bin");
    {
        let mut file = t(File::create(&path));
        // Alternate one block of data with a large hole for each range.
        for i in 0..ranges {
            let offset = (i as u64 + 1) * 2 * 1024 * 1024;
            t(file.seek(io::SeekFrom::Start(offset)));
            t(file.write_all(b"x"));
        }
        file.sync_all().ok();
    }

    let output = work.path().join("sparse.tar");
    let status = t(Command::new("tar")
        .arg("-cf")
        .arg(&output)
        .arg("--sparse")
        .arg("-C")
        .arg(work.path())
        .arg("sparse.bin")
        .status());
    assert!(status.success(), "failed to create sparse tar");
    t(fs::read(&output))
}

#[test]
fn sparse_range_limit_is_enforced() {
    if Command::new("tar").arg("--version").status().is_err() {
        return;
    }
    let bytes = gnu_sparse_archive(5);

    // Sanity check: the archive parses and is recognized as sparse.
    let mut probe = Archive::new(Cursor::new(bytes.clone()));
    let entry = t(t(probe.entries()).next().unwrap());
    assert!(entry.header().entry_type().is_gnu_sparse());

    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(u64::MAX, u64::MAX, 1.0, 2)));

    let err = drain(&mut ar).expect_err("sparse range limit must be enforced");
    assert_eq!(
        budget_error_kind(&err),
        BudgetErrorKind::SparseRangesLimitExceeded
    );
}

#[test]
fn sparse_archive_within_budget_unpacks() {
    if Command::new("tar").arg("--version").status().is_err() {
        return;
    }
    let bytes = gnu_sparse_archive(3);
    let td = t(TempBuilder::new().prefix("tar-budget").tempdir());

    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(
        u64::MAX,
        64 * 1024 * 1024,
        1.0,
        16,
    )));
    t(ar.unpack(td.path()));
    assert!(td.path().join("sparse.bin").exists());
}

#[test]
fn budget_counts_logical_size_of_sparse_holes() {
    if Command::new("tar").arg("--version").status().is_err() {
        return;
    }
    // A roughly 12 MiB logical sparse file stored in a tiny archive. The
    // expanded size must count toward the byte budget even though almost all
    // of it is encoded as holes rather than bytes on disk.
    let bytes = gnu_sparse_archive(5);
    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(
        u64::MAX,
        1024 * 1024,
        1.0,
        u64::MAX,
    )));

    let err = drain(&mut ar).expect_err("sparse logical size must count toward the byte budget");
    // A 12 MiB logical file blows through a 1 MiB budget; either the per-file
    // cap or the cumulative cap may report it depending on the fraction.
    assert!(matches!(
        budget_error_kind(&err),
        BudgetErrorKind::FileSizeLimitExceeded | BudgetErrorKind::TotalBytesLimitExceeded
    ));
}

#[test]
fn cumulative_total_budget_across_entries() {
    // Two 100-byte files against a 150-byte total budget; each individual
    // file fits, but their cumulative expanded size does not.
    let bytes = archive_of_files(2, 100);
    let mut ar = Archive::new(Cursor::new(bytes));
    ar.set_unpack_budget(t(UnpackBudget::with_limits(u64::MAX, 150, 1.0, u64::MAX)));

    let err = drain(&mut ar).expect_err("cumulative size must count toward the byte budget");
    assert_eq!(
        budget_error_kind(&err),
        BudgetErrorKind::TotalBytesLimitExceeded
    );
}
