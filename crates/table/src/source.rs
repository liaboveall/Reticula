//! Where the bytes of a table come from and go to.

/// Why bytes could not be had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// They exist but are not here yet. Whoever asked may ask again later.
    Missing,
    /// They cannot be read.
    Failed,
}

/// A run of bytes of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRange {
    /// Offset of the first byte.
    pub at: u64,
    /// Number of bytes.
    pub len: u64,
}

impl ByteRange {
    /// Offset after the last byte.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.at.saturating_add(self.len)
    }
}

/// A file to read from.
pub trait Source {
    /// Fills `into` with the bytes from `at` on.
    ///
    /// # Errors
    ///
    /// [`SourceError`] when not all of them can be had; what `into` holds
    /// then is unspecified.
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError>;
}

/// Any source. Code that is generic over its source is the same for all that
/// come as this, which keeps it small.
pub struct AnySource<'a>(pub &'a mut dyn Source);

impl core::fmt::Debug for AnySource<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AnySource")
    }
}

impl Source for AnySource<'_> {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        self.0.read(at, into)
    }
}

impl Source for &[u8] {
    fn read(&mut self, at: u64, into: &mut [u8]) -> Result<(), SourceError> {
        let start = usize::try_from(at).map_err(|_| SourceError::Failed)?;
        let end = start.checked_add(into.len()).ok_or(SourceError::Failed)?;
        into.copy_from_slice(self.get(start..end).ok_or(SourceError::Failed)?);
        Ok(())
    }
}

/// A file being written, from its start to its end.
pub trait Sink {
    /// Adds bytes at the end.
    ///
    /// # Errors
    ///
    /// [`SinkFailed`] when they could not be written; the file is then of no use.
    fn append(&mut self, bytes: &[u8]) -> Result<(), SinkFailed>;
}

/// A file could not be written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinkFailed;

impl Sink for alloc::vec::Vec<u8> {
    fn append(&mut self, bytes: &[u8]) -> Result<(), SinkFailed> {
        self.try_reserve(bytes.len()).map_err(|_| SinkFailed)?;
        self.extend_from_slice(bytes);
        Ok(())
    }
}
