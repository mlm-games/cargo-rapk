use std::io::{Cursor, Write};
use zip::{
    CompressionMethod, DateTime, ZipArchive, ZipWriter,
    write::{ExtendedFileOptions, FileOptions},
};

/// Normalize a ZIP: set deterministic mtimes, strip variable extra fields, and
/// write entries in lexicographic order for both local headers and central dir.
pub fn normalize_zip_in_place(
    path: std::path::PathBuf,
    ts: Option<u64>,
) -> Result<(), std::io::Error> {
    let data = std::fs::read(&path)?;
    let normalized = normalize_zip(&data, ts)?;
    std::fs::write(&path, normalized)?;
    Ok(())
}

pub fn normalize_zip(data: &[u8], _ts: Option<u64>) -> Result<Vec<u8>, std::io::Error> {
    let mut src = ZipArchive::new(Cursor::new(data))?;

    // Deterministic order: lexicographic filenames
    let mut names: Vec<String> = (0..src.len())
        .filter_map(|i| src.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    names.sort();

    // Fixed DOS time (1980-01-01 00:00:00)
    let dos_time = DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("valid DOS datetime");

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
