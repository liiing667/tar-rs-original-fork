use std::error;
use std::fmt;
use std::io;

/// Default limit on the number of entries a single archive may contain.
///
/// This bounds the number of filesystem objects (and therefore inodes) that
/// unpacking an archive can create.
pub const DEFAULT_MAX_ENTRIES: u64 = 1_000_000;

/// Default limit on the cumulative number of bytes an archive may expand to.
///
/// Sparse files are counted by their logical (expanded) size so that archives
/// which encode large holes cannot allocate unbounded space while remaining
/// small on disk.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024; // 1 TiB

/// Default limit on the number of sparse ranges a single sparse entry may
/// describe.
pub const DEFAULT_MAX_SPARSE_RANGES: u64 = 1024;

/// Default fraction of the total byte budget that a single entry is allowed to
/// consume.
pub const DEFAULT_MAX_FILE_FRACTION: f64 = 0.95;

/// The reason an [`UnpackBudget`] was exceeded while reading an archive.
///
/// Every budget violation is reported as an `io::Error` carrying an
/// [`UnpackBudgetError`], whose [`UnpackBudgetError::kind`] method returns one
/// of these values. This lets callers distinguish resource-exhaustion errors
/// from ordinary I/O failures while keeping the error type stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BudgetErrorKind {
    /// The archive contained more entries than
    /// [`UnpackBudget::max_entries`] allows.
    EntryLimitExceeded,
    /// The cumulative expanded size of the archive's entries exceeded
    /// [`UnpackBudget::max_total_bytes`].
    TotalBytesLimitExceeded,
    /// A single entry was larger than the fraction of
    /// [`UnpackBudget::max_total_bytes`] allowed by
    /// [`UnpackBudget::max_file_fraction`].
    FileSizeLimitExceeded,
    /// A sparse entry described more ranges than
    /// [`UnpackBudget::max_sparse_ranges`] allows.
    SparseRangesLimitExceeded,
}

impl BudgetErrorKind {
    fn message(self) -> &'static str {
        match self {
            BudgetErrorKind::EntryLimitExceeded => {
                "archive contains more entries than the unpack budget allows"
            }
            BudgetErrorKind::TotalBytesLimitExceeded => {
                "archive expands to more bytes than the unpack budget allows"
            }
            BudgetErrorKind::FileSizeLimitExceeded => {
                "a single entry is larger than the unpack budget allows"
            }
            BudgetErrorKind::SparseRangesLimitExceeded => {
                "a sparse entry describes more ranges than the unpack budget allows"
            }
        }
    }
}

impl fmt::Display for BudgetErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl error::Error for BudgetErrorKind {}

/// An error returned when an archive violates its configured [`UnpackBudget`].
///
/// Budget violations surface as ordinary [`io::Error`] values (so they compose
/// with the rest of the crate's APIs) whose
/// [`std::error::Error::source`] is an `UnpackBudgetError`. The concrete
/// reason is available via [`UnpackBudgetError::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnpackBudgetError {
    kind: BudgetErrorKind,
}

impl UnpackBudgetError {
    /// Returns the specific budget that was exceeded.
    pub fn kind(&self) -> BudgetErrorKind {
        self.kind
    }
}

impl fmt::Display for UnpackBudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.message())
    }
}

impl error::Error for UnpackBudgetError {}

/// Limits applied while reading or unpacking an archive to keep resource
/// consumption of untrusted archives bounded.
///
/// An `UnpackBudget` bounds the work that a single archive can perform:
///
/// * [`max_entries`](UnpackBudget::max_entries) limits the number of entries,
///   which bounds inode consumption when unpacking.
/// * [`max_total_bytes`](UnpackBudget::max_total_bytes) limits the cumulative
///   expanded size of all entries (sparse holes are counted at their logical
///   size).
/// * [`max_file_fraction`](UnpackBudget::max_file_fraction) limits how much of
///   the total byte budget a single entry may consume.
/// * [`max_sparse_ranges`](UnpackBudget::max_sparse_ranges) limits the number
///   of ranges a single sparse entry may describe.
///
/// A budget is enforced as soon as the relevant header has been parsed, before
/// any entry data is read or written. As a result the limits apply both to
/// [`Archive::unpack`](crate::Archive::unpack) and to manually iterating over
/// [`Archive::entries`](crate::Archive::entries).
///
/// Every [`Archive`](crate::Archive) starts with the conservative
/// [`UnpackBudget::default`]. Use [`Archive::set_unpack_budget`](
/// crate::Archive::set_unpack_budget) to relax or tighten the limits, or pass
/// [`UnpackBudget::unlimited`] to disable them.
#[derive(Clone, Debug)]
pub struct UnpackBudget {
    max_entries: u64,
    max_total_bytes: u64,
    max_file_fraction: f64,
    max_sparse_ranges: u64,
}

impl Default for UnpackBudget {
    fn default() -> UnpackBudget {
        UnpackBudget {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_file_fraction: DEFAULT_MAX_FILE_FRACTION,
            max_sparse_ranges: DEFAULT_MAX_SPARSE_RANGES,
        }
    }
}

impl UnpackBudget {
    /// Creates a budget that places no limits on an archive.
    ///
    /// This restores the behavior of archives without a budget and should only
    /// be used when the archive is trusted or its size is otherwise bounded.
    pub fn unlimited() -> UnpackBudget {
        UnpackBudget {
            max_entries: u64::MAX,
            max_total_bytes: u64::MAX,
            max_file_fraction: 1.0,
            max_sparse_ranges: u64::MAX,
        }
    }

    /// Creates a budget with explicit limits for every dimension.
    ///
    /// `max_file_fraction` must be between `0.0` and `1.0` (inclusive);
    /// otherwise an error is returned. Pass [`u64::MAX`] for a dimension that
    /// should not be limited.
    pub fn with_limits(
        max_entries: u64,
        max_total_bytes: u64,
        max_file_fraction: f64,
        max_sparse_ranges: u64,
    ) -> io::Result<UnpackBudget> {
        if !(0.0..=1.0).contains(&max_file_fraction) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_file_fraction must be between 0.0 and 1.0",
            ));
        }
        Ok(UnpackBudget {
            max_entries,
            max_total_bytes,
            max_file_fraction,
            max_sparse_ranges,
        })
    }

    /// Returns the maximum number of entries allowed in an archive.
    pub fn max_entries(&self) -> u64 {
        self.max_entries
    }

    /// Sets the maximum number of entries allowed in an archive.
    pub fn set_max_entries(&mut self, max_entries: u64) -> &mut UnpackBudget {
        self.max_entries = max_entries;
        self
    }

    /// Returns the maximum cumulative expanded size, in bytes, of all entries.
    pub fn max_total_bytes(&self) -> u64 {
        self.max_total_bytes
    }

    /// Sets the maximum cumulative expanded size, in bytes, of all entries.
    pub fn set_max_total_bytes(&mut self, max_total_bytes: u64) -> &mut UnpackBudget {
        self.max_total_bytes = max_total_bytes;
        self
    }

    /// Returns the fraction of the total byte budget that a single entry is
    /// allowed to consume.
    pub fn max_file_fraction(&self) -> f64 {
        self.max_file_fraction
    }

    /// Sets the fraction of the total byte budget that a single entry is
    /// allowed to consume.
    ///
    /// `max_file_fraction` must be between `0.0` and `1.0` (inclusive);
    /// otherwise an error is returned and the budget is left unchanged.
    pub fn set_max_file_fraction(
        &mut self,
        max_file_fraction: f64,
    ) -> io::Result<&mut UnpackBudget> {
        if !(0.0..=1.0).contains(&max_file_fraction) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_file_fraction must be between 0.0 and 1.0",
            ));
        }
        self.max_file_fraction = max_file_fraction;
        Ok(self)
    }

    /// Returns the maximum number of sparse ranges a single sparse entry may
    /// describe.
    pub fn max_sparse_ranges(&self) -> u64 {
        self.max_sparse_ranges
    }

    /// Sets the maximum number of sparse ranges a single sparse entry may
    /// describe.
    pub fn set_max_sparse_ranges(&mut self, max_sparse_ranges: u64) -> &mut UnpackBudget {
        self.max_sparse_ranges = max_sparse_ranges;
        self
    }

    /// Returns the maximum size, in bytes, that a single entry may have given
    /// the configured total byte budget and file fraction.
    pub(crate) fn max_file_bytes(&self) -> u64 {
        let bytes = self.max_total_bytes as f64 * self.max_file_fraction;
        if bytes >= u64::MAX as f64 {
            u64::MAX
        } else {
            bytes as u64
        }
    }
}

pub(crate) fn budget_error(kind: BudgetErrorKind) -> io::Error {
    // `io::Error::other` was only stabilized in Rust 1.74; keep the older
    // constructor so this crate keeps building on its existing MSRV.
    #[allow(clippy::io_other_error)]
    io::Error::new(io::ErrorKind::Other, UnpackBudgetError { kind })
}
