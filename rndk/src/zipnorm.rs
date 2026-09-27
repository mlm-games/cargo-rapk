use crate::error::NdkError;
use std::io::{Cursor, Write};
use time::{OffsetDateTime, PrimitiveDateTime};
use zip::{
    CompressionMethod, DateTime, ZipArchive, ZipWriter,
    write::{ExtendedFileOptions, FileOptions},
};

/// The DOS epoch, the earliest timestamp a zip entry can represent, and the
/// default for a reproducible build.
pub fn dos_epoch() -> Result<DateTime, NdkError> {
    DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .map_err(|_| NdkError::TimestampOutOfRange(315_532_800))
}

/// Convert a Unix timestamp (seconds since epoch) to a DOS [`DateTime`], which
/// cannot represent anything outside 1980-2107.
pub fn unix_ts_to_dos(ts: u64) -> Result<DateTime, NdkError> {
    let ts = i64::try_from(ts).map_err(|_| NdkError::TimestampOutOfRange(i64::MAX))?;
    let out_of_range = || NdkError::TimestampOutOfRange(ts);
    let odt = OffsetDateTime::from_unix_timestamp(ts).map_err(|_| out_of_range())?;
    let pdt = PrimitiveDateTime::new(odt.date(), odt.time());
    DateTime::try_from(pdt).map_err(|_| out_of_range())
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
