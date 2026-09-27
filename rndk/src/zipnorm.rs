use crate::error::NdkError;
use std::io::{Cursor, Write};
use time::{OffsetDateTime, PrimitiveDateTime};
use zip::{
    CompressionMethod, DateTime, ZipArchive, ZipWriter,
    write::{ExtendedFileOptions, FileOptions},
};

/// 1980-01-01T00:00:00Z: the DOS epoch, and the earliest timestamp a zip entry
/// can record. A DOS date is a 7-bit year offset from 1980, so anything earlier
/// is simply not representable.
const DOS_EPOCH_UNIX: i64 = 315_532_800;

/// The DOS epoch, the earliest timestamp a zip entry can represent, and the
/// default for a reproducible build.
pub fn dos_epoch() -> Result<DateTime, NdkError> {
    DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .map_err(|_| NdkError::TimestampOutOfRange(DOS_EPOCH_UNIX))
}

/// Convert a Unix timestamp (seconds since epoch) to a DOS [`DateTime`], which
/// cannot represent anything outside 1980-2107.
///
/// Timestamps before 1980 are clamped to the DOS epoch rather than rejected,
/// so the conventional `SOURCE_DATE_EPOCH=0` behaves as "as early as the format
/// allows". Timestamps past 2107 are rejected, because there is no correct
/// value to clamp them to.
pub fn unix_ts_to_dos(ts: u64) -> Result<DateTime, NdkError> {
    let ts = i64::try_from(ts).map_err(|_| NdkError::TimestampOutOfRange(i64::MAX))?;
    let ts = ts.max(DOS_EPOCH_UNIX);
    let odt =
        OffsetDateTime::from_unix_timestamp(ts).map_err(|_| NdkError::TimestampOutOfRange(ts))?;
    let pdt = PrimitiveDateTime::new(odt.date(), odt.time());
    DateTime::try_from(pdt).map_err(|_| NdkError::TimestampOutOfRange(ts))
}

/// Normalize a ZIP: set deterministic mtimes, strip variable extra fields, and
/// write entries in lexicographic order for both local headers and central dir.
pub fn normalize_zip_in_place(path: std::path::PathBuf, ts: Option<u64>) -> Result<(), NdkError> {
    let data = std::fs::read(&path).map_err(|e| NdkError::IoPathError(path.clone(), e))?;
    let normalized = normalize_zip(&data, ts)?;
    std::fs::write(&path, normalized).map_err(|e| NdkError::IoPathError(path, e))?;
    Ok(())
}

pub fn normalize_zip(data: &[u8], ts: Option<u64>) -> Result<Vec<u8>, NdkError> {
    let mut src = ZipArchive::new(Cursor::new(data))?;

    // Deterministic order: lexicographic filenames
    let mut names: Vec<String> = (0..src.len())
        .filter_map(|i| src.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    names.sort();

    // Use the provided timestamp, or fall back to the DOS epoch
    let dos_time = match ts {
        Some(ts) => unix_ts_to_dos(ts)?,
        None => dos_epoch()?,
    };

    let cursor = Cursor::new(Vec::with_capacity(data.len()));
    let mut writer = ZipWriter::new(cursor);

    for name in names {
        let mut file = src.by_name(&name)?;

        let method = match file.compression() {
            CompressionMethod::Stored => CompressionMethod::Stored,
            _ => CompressionMethod::Deflated,
        };

        let mut buf = Vec::with_capacity(file.size() as usize);
        std::io::copy(&mut file, &mut buf)?;

        let mut opts: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
            .compression_method(method)
            .last_modified_time(dos_time);

        if file.size() > 0xFFFF_FFFF {
            opts = opts.large_file(true);
        }

        writer.start_file(name, opts)?;
        writer.write_all(&buf)?;
    }

    let cursor = writer.finish()?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dos(ts: u64) -> (u16, u8, u8) {
        let dt = unix_ts_to_dos(ts).expect("should convert");
        (dt.year(), dt.month(), dt.day())
    }

    #[test]
    fn timestamps_before_the_dos_epoch_clamp() {
        // The conventional reproducible-build value must not be an error.
        assert_eq!(dos(0), (1980, 1, 1));
        assert_eq!(dos(1), (1980, 1, 1));
        assert_eq!(dos(DOS_EPOCH_UNIX as u64 - 1), (1980, 1, 1));
    }

    #[test]
    fn timestamps_from_the_dos_epoch_onward_are_exact() {
        assert_eq!(dos(DOS_EPOCH_UNIX as u64), (1980, 1, 1));
        assert_eq!(dos(1_700_000_000), (2023, 11, 14));
    }

    #[test]
    fn timestamps_past_2107_are_rejected() {
        // 2107-12-31 is the last representable year; 2108-01-01 is not.
        assert!(unix_ts_to_dos(4_354_732_800).is_ok());
        assert!(matches!(
            unix_ts_to_dos(4_354_819_200),
            Err(NdkError::TimestampOutOfRange(_))
        ));
        assert!(matches!(
            unix_ts_to_dos(u64::MAX),
            Err(NdkError::TimestampOutOfRange(_))
        ));
    }
}
