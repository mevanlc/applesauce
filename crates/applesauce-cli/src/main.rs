use crate::progress::{ProgressBarWriter, ProgressBars, Verbosity};
#[cfg(feature = "lzfse")]
use applesauce::compressor::LzfseBackend;
use applesauce::compressor::{Encoder, Kind};
use applesauce::{info, Stats};
use cfg_if::cfg_if;
use clap::Parser;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufWriter, LineWriter};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::{fmt, io};
use tracing::metadata::LevelFilter;
use tracing_chrome::ChromeLayerBuilder;
use tracing_subscriber::fmt::time;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

mod progress;

#[derive(Debug, clap::Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Output chrome tracing format to a file
    ///
    /// The passed file can be passed to chrome at chrome://tracing
    #[arg(long, global(true))]
    chrome_tracing: Option<PathBuf>,

    #[arg(short, long, global(true), action = clap::ArgAction::Count)]
    verbose: u8,

    /// Reduce output
    ///
    /// Repeat to suppress the final compression summary and warnings about ignored levels
    #[arg(short, long, global(true), action = clap::ArgAction::Count, conflicts_with = "verbose")]
    quiet: u8,
}

impl Cli {
    fn verbosity(&self) -> Verbosity {
        let verbosity = self.verbose as i8 - self.quiet as i8;
        match verbosity {
            ..=-1 => Verbosity::Quiet,
            0 => Verbosity::Normal,
            1.. => Verbosity::Verbose,
        }
    }

    fn show_compression_summary(&self) -> bool {
        self.quiet < 2
    }
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Compress files
    Compress(Compress),

    /// Decompress files
    #[command(alias = "uncompress")]
    Decompress(Decompress),

    /// Get info about compression for file(s)
    Info(Info),
}

#[derive(Debug, clap::Args)]
struct Decompress {
    /// Paths to recursively decompress
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Decompress manually, rather than allowing the OS to do decompression
    ///
    /// This may be useful to do decompression from an older OS version which can't
    /// natively read the compressed file
    #[arg(long)]
    manual: bool,

    /// Verify that the decompressed file has the same contents as the original before replacing it
    ///
    /// This is an extra safety check to ensure that the decompressed file is exactly the same as the
    /// original file.
    #[arg(long)]
    verify: bool,

    #[command(flatten)]
    scratch_options: ScratchOptions,
}

const DEFAULT_COMPRESSION_LEVEL: u32 = 5;

#[derive(Debug, clap::Args)]
struct Compress {
    /// Paths to recursively compress
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// ZLIB compression level, 1-12 (default: 5)
    ///
    /// Specifying a level for LZFSE or LZVN emits a warning because it has no effect,
    /// unless -qq is used.
    #[arg(
        short, long,
        value_parser = clap::value_parser!(u32).range(1..=12)
    )]
    level: Option<u32>,

    /// The minimum compression ratio
    ///
    /// Files will be skipped if they compress to a larger size than this ratio
    /// of the original size
    ///
    /// A value of 0.0 (or less) will skip all files
    /// A value of 1.0 will only skip files which cannot be compressed at all
    /// Values greater than 1.0 are valid, and will allow forcing compression to
    /// be used even if it results in a larger file
    #[arg(short = 'r', long, default_value_t = 0.95)]
    minimum_compression_ratio: f64,

    /// The type of compression to use
    #[arg(short, long, value_enum, default_value_t = Compression::default())]
    compression: Compression,

    /// LZFSE encoder implementation (only valid with -c lzfse)
    #[cfg(feature = "lzfse")]
    #[arg(short, long, value_enum, help = backend_help())]
    backend: Option<Backend>,

    /// Verify that the compressed file has the same contents as the original before replacing it
    ///
    /// This is an extra safety check to ensure that the compressed file is exactly the same as the
    /// original file.
    #[arg(long)]
    verify: bool,

    #[command(flatten)]
    scratch_options: ScratchOptions,
}

impl Compress {
    fn encoder(&self) -> Result<Encoder, String> {
        let kind = Kind::from(self.compression);
        #[cfg(feature = "lzfse")]
        let encoder = match self.backend {
            Some(backend) if kind == Kind::Lzfse => Encoder::lzfse(backend.into()),
            Some(_) => return Err("--backend is only valid with -c lzfse".to_owned()),
            None => kind.into(),
        };
        #[cfg(not(feature = "lzfse"))]
        let encoder = Encoder::from(kind);
        Ok(encoder)
    }
}

#[cfg(feature = "lzfse")]
fn backend_help() -> String {
    use clap::ValueEnum;
    format!(
        "LZFSE encoder implementation (LZFSE only; default: {})",
        Backend::default().to_possible_value().unwrap().get_name()
    )
}

#[cfg(feature = "lzfse")]
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
enum Backend {
    /// Apple's macOS compression library
    #[cfg(target_os = "macos")]
    Macos,
    /// Unmodified lzfse-sys encoder
    Crate,
    /// Vendored encoder with stock settings
    Vendor,
    /// Vendored encoder tuned for higher compression, using more time and memory
    VendorUltra,
}

#[cfg(feature = "lzfse")]
impl From<Backend> for LzfseBackend {
    fn from(backend: Backend) -> Self {
        match backend {
            #[cfg(target_os = "macos")]
            Backend::Macos => Self::Macos,
            Backend::Crate => Self::Crate,
            Backend::Vendor => Self::Vendor,
            Backend::VendorUltra => Self::VendorUltra,
        }
    }
}

#[cfg(feature = "lzfse")]
impl Default for Backend {
    fn default() -> Self {
        match LzfseBackend::default() {
            #[cfg(target_os = "macos")]
            LzfseBackend::Macos => Self::Macos,
            LzfseBackend::Vendor => Self::Vendor,
            LzfseBackend::VendorUltra => Self::VendorUltra,
            _ => Self::Crate,
        }
    }
}

#[derive(Debug, clap::Args)]
struct ScratchOptions {
    /// Stage output in this directory, then copy completed files to their destination one at a time
    #[arg(long, value_name = "DIR")]
    scratch: Option<PathBuf>,

    /// Maximum scratch backlog, including reservations for files being processed (default: 16GiB)
    ///
    /// Accepts bytes or integer sizes such as 512MiB, 16GiB, or 16GB.
    /// Files whose worst-case output exceeds the limit are left unchanged.
    /// Decompression reserves the full uncompressed size of each file.
    /// Filesystem overhead and destination temporary files are excluded.
    #[arg(long, value_name = "SIZE", requires = "scratch", value_parser = parse_scratch_limit)]
    scratch_limit: Option<u64>,
}

const DEFAULT_SCRATCH_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

impl ScratchOptions {
    fn file_compressor(self) -> applesauce::FileCompressor {
        match self.scratch {
            Some(directory) => applesauce::FileCompressor::with_scratch(
                &directory,
                self.scratch_limit.unwrap_or(DEFAULT_SCRATCH_LIMIT),
            )
            .unwrap_or_else(|error| {
                eprintln!(
                    "Unable to use scratch directory {}: {error}",
                    directory.display()
                );
                std::process::exit(1);
            }),
            None => applesauce::FileCompressor::new(),
        }
    }
}

fn parse_scratch_limit(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split]
        .parse::<u64>()
        .map_err(|_| "expected a positive integer size, such as 16GiB".to_owned())?;
    let multiplier: u64 = match value[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024_u64.pow(2),
        "gib" => 1024_u64.pow(3),
        "tib" => 1024_u64.pow(4),
        "kb" => 1000,
        "mb" => 1000_u64.pow(2),
        "gb" => 1000_u64.pow(3),
        "tb" => 1000_u64.pow(4),
        _ => {
            return Err(
                "expected bytes or a B, KB, MB, GB, TB, KiB, MiB, GiB, or TiB suffix".to_owned(),
            )
        }
    };
    number
        .checked_mul(multiplier)
        .filter(|&n| n > 0)
        .ok_or_else(|| "scratch limit must be greater than zero and fit in 64 bits".to_owned())
}

#[derive(Debug, clap::Args)]
struct Info {
    /// Paths to inspect
    ///
    /// Info will be reported for each path unless --summary is used
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Report a single summary across all paths
    #[arg(long)]
    summary: bool,
}

#[derive(Debug, Copy, Clone, clap::ValueEnum, PartialEq, Eq)]
enum Compression {
    #[cfg(feature = "lzfse")]
    Lzfse,
    #[cfg(feature = "zlib")]
    Zlib,
    #[cfg(feature = "lzvn")]
    Lzvn,
}

impl From<Compression> for Kind {
    fn from(c: Compression) -> Self {
        match c {
            #[cfg(feature = "zlib")]
            Compression::Zlib => Kind::Zlib,
            #[cfg(feature = "lzfse")]
            Compression::Lzfse => Kind::Lzfse,
            #[cfg(feature = "lzvn")]
            Compression::Lzvn => Kind::Lzvn,
        }
    }
}

// Using cfg_if is much easier than using drive and specifying the default as an attribute on a derive
#[allow(clippy::derivable_impls)]
impl Default for Compression {
    fn default() -> Self {
        cfg_if! {
            if #[cfg(feature = "lzfse")] {
                Self::Lzfse
            } else if #[cfg(feature = "zlib")] {
                Self::Zlib
            } else if #[cfg(feature = "lzvn")] {
                Self::Lzvn
            } else {
                compile_error!("At least one compression type must be configured")
            }
        }
    }
}

fn chrome_tracing_file(path: Option<&Path>) -> Option<impl io::Write> {
    let path = path?;

    let file = match File::create(path) {
        Ok(file) => file,
        Err(e) => {
            // Tracing isn't set up yet, log the old-fashioned way
            eprintln!("Unable to open chrome layer: {e}");
            return None;
        }
    };

    Some(BufWriter::new(file))
}

fn main() {
    let cli = Cli::parse();
    if let Commands::Compress(options) = &cli.command {
        if let Err(message) = options.encoder() {
            use clap::CommandFactory;
            Cli::command()
                .error(clap::error::ErrorKind::ValueValidation, message)
                .exit();
        }
    }
    let verbosity = cli.verbosity();
    let show_compression_summary = cli.show_compression_summary();

    let mut _chrome_guard = None;
    let chrome_file = chrome_tracing_file(cli.chrome_tracing.as_deref());
    let chrome_layer: Option<_> = chrome_file.map(|f| {
        let (layer, guard) = ChromeLayerBuilder::new()
            .writer(f)
            .include_args(true)
            .build();
        _chrome_guard = Some(guard);
        layer
    });

    let progress_bars = match &cli.command {
        Commands::Info(_) => ProgressBars::for_info(cli.verbosity()),
        Commands::Compress(_) | Commands::Decompress(_) => ProgressBars::new(cli.verbosity()),
    };
    let fmt_writer = Mutex::new(LineWriter::new(ProgressBarWriter::new(
        progress_bars.multi_progress().clone(),
        io::stderr(),
    )));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_timer(time::uptime())
        .with_writer(fmt_writer)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::OFF.into())
                .from_env_lossy(),
        );

    tracing_subscriber::registry()
        .with(chrome_layer)
        .with(fmt_layer)
        .init();

    match cli.command {
        Commands::Compress(options) => {
            let encoder = options.encoder().expect("validated compression options");
            let Compress {
                paths,
                minimum_compression_ratio,
                level,
                verify,
                scratch_options,
                ..
            } = options;

            if let Some(level) = level.filter(|_| cli.quiet < 2 && !encoder.supports_level()) {
                eprintln!("Warning: --level {level} has no effect for the selected encoder");
            }

            let mut compressor = scratch_options.file_compressor();
            let stats = compressor.recursive_compress(
                paths.iter().map(Path::new),
                encoder,
                minimum_compression_ratio,
                level.unwrap_or(DEFAULT_COMPRESSION_LEVEL),
                &progress_bars,
                verify,
            );
            progress_bars.finish();
            tracing::info!("Finished compressing");
            if show_compression_summary {
                display_stats(&stats, true);
            }
        }
        Commands::Decompress(Decompress {
            paths,
            manual,
            verify,
            scratch_options,
        }) => {
            let mut compressor = scratch_options.file_compressor();
            let stats = compressor.recursive_decompress(
                paths.iter().map(Path::new),
                manual,
                &progress_bars,
                verify,
            );
            progress_bars.finish();
            tracing::info!("Finished decompressing");
            if verbosity >= Verbosity::Normal {
                display_stats(&stats, false);
            }
        }
        Commands::Info(Info { paths, summary }) => {
            if summary {
                let mut summary_info = info::AfscFolderInfo::default();
                for path in paths {
                    match info::get_recursive_with_progress(&path, &progress_bars) {
                        Ok(path_info) => add_folder_info(&mut summary_info, &path_info),
                        Err(e) => tracing::error!(
                            "error reading compression info for {}: {}",
                            path.display(),
                            e,
                        ),
                    }
                }
                progress_bars.finish();
                display_folder_summary(&summary_info);
                return;
            }

            for path in paths {
                if path.is_dir() {
                    let info = info::get_recursive_with_progress(&path, &progress_bars);
                    let info = match info {
                        Ok(info) => info,
                        Err(e) => {
                            tracing::error!(
                                "error reading compression info for {}: {}",
                                path.display(),
                                e,
                            );
                            continue;
                        }
                    };
                    progress_bars
                        .multi_progress()
                        .suspend(|| display_folder_info(&path, &info));
                } else {
                    let info = info::get_with_progress(&path, &progress_bars);
                    let info = match info {
                        Ok(info) => info,
                        Err(e) => {
                            tracing::error!(
                                "error reading compression info for {}: {}",
                                path.display(),
                                e,
                            );
                            continue;
                        }
                    };
                    progress_bars
                        .multi_progress()
                        .suspend(|| display_file_info(&path, &info));
                }
            }
            progress_bars.finish();
        }
    }
}

fn display_folder_info(path: &Path, info: &info::AfscFolderInfo) {
    println!("\n{}:", path.display());

    display_folder_summary(info);
}

fn display_folder_summary(info: &info::AfscFolderInfo) {
    println!("-- Files --");
    println!("Compressed files:         {}", info.num_compressed_files);
    println!("Total files:              {}", info.num_files);
    println!("Total folders:            {}", info.num_folders);

    println!();
    println!("-- Storage --");
    println!(
        "Logical size:             {} ({})",
        format_bytes(info.total_uncompressed_size),
        info.total_uncompressed_size
    );
    println!(
        "On-disk size:             {} ({})",
        format_bytes(info.total_compressed_size),
        info.total_compressed_size
    );
    println!(
        "Compression savings:      {:.1}%",
        info.compression_savings_fraction() * 100.0,
    );
}

fn add_folder_info(total: &mut info::AfscFolderInfo, info: &info::AfscFolderInfo) {
    total.num_compressed_files += info.num_compressed_files;
    total.num_files += info.num_files;
    total.num_folders += info.num_folders;
    total.total_uncompressed_size += info.total_uncompressed_size;
    total.total_compressed_size += info.total_compressed_size;
}

fn display_file_info(path: &Path, info: &info::AfscFileInfo) {
    if info.is_compressed {
        println!("{} is compressed", path.display());
    } else {
        println!("{} is not compressed", path.display());
    }

    if info.is_compressed {
        println!();
        println!("-- Compression --");
        match &info.decmpfs_info {
            Some(Ok(decmpfs_info)) => {
                println!(
                    "Compression type:         {}",
                    decmpfs_info.compression_type
                );
                println!(
                    "decmpfs logical size:     {} ({})",
                    format_bytes(decmpfs_info.orig_file_size),
                    decmpfs_info.orig_file_size
                );
                println!(
                    "decmpfs xattr size:       {} ({})",
                    format_bytes(decmpfs_info.attribute_size),
                    decmpfs_info.attribute_size
                );
            }
            Some(Err(decmpfs_err)) => {
                tracing::error!(
                    "compressed file has issue with decompfs xattr: {}",
                    decmpfs_err
                );
            }
            None => {
                tracing::error!("compressed file has no decmpfs xattr");
            }
        }
    }

    println!();
    println!("-- Storage --");
    println!(
        "Logical size:             {} ({})",
        format_bytes(info.stat_size),
        info.stat_size
    );
    println!(
        "On-disk size:             {} ({})",
        format_bytes(info.on_disk_size),
        info.on_disk_size
    );
    if info.is_compressed {
        println!(
            "Compression savings:      {:.1}%",
            (1.0 - info.compressed_fraction()) * 100.0
        );
    }

    println!();
    println!("-- Extended Attributes --");
    println!("Extended attributes:      {}", info.xattr_count);
    println!(
        "Extended attribute size:  {} ({})",
        format_bytes(info.total_xattr_size),
        info.total_xattr_size
    );
    if let Some(resource_fork_size) = info.resource_fork_size {
        println!(
            "Resource fork size:       {} ({})",
            format_bytes(resource_fork_size),
            resource_fork_size
        );
    }
}

pub fn display_stats(stats: &Stats, compress_mode: bool) {
    println!("Total Files: {}", stats.files.load(Ordering::Relaxed));
    let total_file_sizes = stats.total_file_sizes.load(Ordering::Relaxed);

    let compressed_count_start = stats.compressed_file_count_start.load(Ordering::Relaxed);
    let compressed_count_final = stats.compressed_file_count_final.load(Ordering::Relaxed);
    let compressed_size_start = stats.compressed_size_start.load(Ordering::Relaxed);
    let compressed_size_final = stats.compressed_size_final.load(Ordering::Relaxed);

    if compress_mode {
        display_compress_stats(
            total_file_sizes,
            compressed_count_start,
            compressed_count_final,
            compressed_size_start,
            compressed_size_final,
        );
    } else {
        display_decompress_stats(
            total_file_sizes,
            compressed_count_start,
            compressed_count_final,
            compressed_size_start,
            compressed_size_final,
        );
    }
}

fn display_compress_stats(
    total_file_sizes: u64,
    compressed_count_start: u64,
    compressed_count_final: u64,
    compressed_size_start: u64,
    compressed_size_final: u64,
) {
    let new_compressed_files = compressed_count_final.saturating_sub(compressed_count_start);
    let existing_saved = saved_bytes(total_file_sizes, compressed_size_start);
    let additional_saved = saved_bytes(compressed_size_start, compressed_size_final);
    let total_saved = saved_bytes(total_file_sizes, compressed_size_final);

    println!("-- Existing Savings --");
    println!("Already compressed files: {compressed_count_start}");
    println!(
        "Starting logical size:    {} ({})",
        format_bytes(total_file_sizes),
        total_file_sizes,
    );
    println!(
        "Starting on-disk size:    {} ({})",
        format_bytes(compressed_size_start),
        compressed_size_start,
    );
    println!(
        "Existing savings:         {:.1}%",
        savings_percent(total_file_sizes, existing_saved)
    );

    println!();
    println!("-- New Savings --");
    println!("New files compressed:     {new_compressed_files}");
    println!(
        "Additional bytes saved:   {} ({additional_saved})",
        format_signed_bytes(additional_saved),
    );
    println!(
        "Additional savings:       {:.1}%",
        savings_percent(compressed_size_start, additional_saved)
    );

    println!();
    println!("-- Total Savings --");
    println!("Total compressed files:   {compressed_count_final}");
    println!(
        "Final on-disk size:       {} ({})",
        format_bytes(compressed_size_final),
        compressed_size_final,
    );
    println!(
        "Total savings:            {:.1}%",
        savings_percent(total_file_sizes, total_saved)
    );
}

fn display_decompress_stats(
    total_file_sizes: u64,
    compressed_count_start: u64,
    compressed_count_final: u64,
    compressed_size_start: u64,
    compressed_size_final: u64,
) {
    let decompressed_files = compressed_count_start.saturating_sub(compressed_count_final);
    let starting_saved = saved_bytes(total_file_sizes, compressed_size_start);
    let final_saved = saved_bytes(total_file_sizes, compressed_size_final);
    let additional_bytes_used = saved_bytes(compressed_size_final, compressed_size_start);
    let savings_removed = savings_percent(total_file_sizes, starting_saved - final_saved);

    println!("-- Starting Savings --");
    println!("Compressed files:         {compressed_count_start}");
    println!(
        "Starting logical size:    {} ({})",
        format_bytes(total_file_sizes),
        total_file_sizes,
    );
    println!(
        "Starting on-disk size:    {} ({})",
        format_bytes(compressed_size_start),
        compressed_size_start,
    );
    println!(
        "Starting savings:         {:.1}%",
        savings_percent(total_file_sizes, starting_saved)
    );

    println!();
    println!("-- Decompression --");
    println!("Files decompressed:       {decompressed_files}");
    println!(
        "Additional bytes used:    {} ({additional_bytes_used})",
        format_signed_bytes(additional_bytes_used),
    );
    println!("Savings lost:             {savings_removed:.1} percentage points");

    println!();
    println!("-- Final Savings --");
    println!("Remaining compressed:     {compressed_count_final}");
    println!(
        "Final on-disk size:       {} ({})",
        format_bytes(compressed_size_final),
        compressed_size_final,
    );
    println!(
        "Final savings:            {:.1}%",
        savings_percent(total_file_sizes, final_saved)
    );
}

fn saved_bytes(original_size: u64, final_size: u64) -> i128 {
    i128::from(original_size) - i128::from(final_size)
}

fn savings_percent(original_size: u64, saved_bytes: i128) -> f64 {
    if original_size == 0 {
        0.0
    } else {
        saved_bytes as f64 / original_size as f64 * 100.0
    }
}

fn format_signed_bytes(byte_size: i128) -> String {
    let sign = if byte_size < 0 { "-" } else { "" };
    format!("{sign}{}", format_bytes(byte_size.unsigned_abs() as u64))
}

#[must_use]
pub fn truncate_path(path: &Path, width: usize) -> PathBuf {
    let mut segments: Vec<_> = path.components().collect();
    let mut total_len = path.as_os_str().len();

    if total_len <= width || segments.len() <= 1 {
        return path.to_owned();
    }

    let mut first = true;
    while total_len > width && segments.len() > 1 {
        // Bias toward the beginning for even counts
        let mid = (segments.len() - 1) / 2;
        let segment = segments[mid];
        if matches!(segment, Component::RootDir | Component::Prefix(_)) {
            break;
        }

        total_len -= segment.as_os_str().len();

        if first {
            // First time, we're just replacing the segment with an ellipsis
            // like `aa/bb/cc/dd` -> `aa/…/cc/dd`, so we remove the
            // segment, and add an ellipsis char
            total_len += 1;
            first = false;
        } else {
            // Other times, we're removing the segment, and a slash
            // `aa/…/cc/dd` -> `aa/…/dd`
            total_len -= 1;
        }
        segments.remove(mid);
    }
    segments.insert(segments.len() / 2, Component::Normal(OsStr::new("…")));
    let mut path = PathBuf::with_capacity(total_len);
    for segment in segments {
        path.push(segment);
    }

    path
}

fn format_bytes(byte_size: u64) -> impl fmt::Display {
    humansize::SizeFormatter::new(byte_size, humansize::BINARY)
}

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[test]
fn minimal_truncate() {
    let orig_path = Path::new("abcd");
    // Trying to truncate smaller than a single segment does nothing
    assert_eq!(truncate_path(orig_path, 1), PathBuf::from("abcd"));

    let orig_path = Path::new("1234/5678");
    // Trying to truncate removes the first element
    assert_eq!(truncate_path(orig_path, 1), PathBuf::from("…/5678"));
    let orig_path = Path::new("/1234/5678");
    // Never truncate the leading /
    assert_eq!(truncate_path(orig_path, 1), PathBuf::from("/…/5678"));

    let orig_path = Path::new("/1234/5678");
    assert_eq!(truncate_path(orig_path, 1), PathBuf::from("/…/5678"));

    let orig_path = Path::new("/1234/5678/90123/4567");
    assert_eq!(truncate_path(orig_path, 1), PathBuf::from("/…/4567"));
}

#[test]
fn no_truncation() {
    let orig_path = Path::new("abcd");
    assert_eq!(truncate_path(orig_path, 4), PathBuf::from(orig_path));

    let orig_path = Path::new("a/b/c/d");
    assert_eq!(truncate_path(orig_path, 7), PathBuf::from(orig_path));
    let orig_path = Path::new("/a/b/c/d");
    assert_eq!(truncate_path(orig_path, 8), PathBuf::from(orig_path));
}

#[test]
fn truncate_single_segment() {
    let orig_path = Path::new("a/bbbbbbbbbb/c");
    assert_eq!(truncate_path(orig_path, 5), PathBuf::from("a/…/c"));
}

#[test]
fn command_check() {
    use clap::CommandFactory;
    Cli::command().debug_assert()
}

#[cfg(feature = "lzfse")]
#[test]
fn lzfse_backend_and_level_selection() {
    let mut backends = vec![
        ("crate", LzfseBackend::Crate),
        ("vendor", LzfseBackend::Vendor),
        ("vendor-ultra", LzfseBackend::VendorUltra),
    ];
    #[cfg(target_os = "macos")]
    backends.push(("macos", LzfseBackend::Macos));
    for (name, backend) in backends {
        let cli = Cli::try_parse_from(["applesauce", "compress", "-c", "lzfse", "-b", name, "."])
            .unwrap();
        let Commands::Compress(options) = cli.command else {
            panic!()
        };
        assert_eq!(options.encoder().unwrap(), Encoder::lzfse(backend));
    }
    for (args, valid) in [
        (vec!["-b", "vendor", "-l9"], true),
        (vec!["-b", "vendor-ultra"], true),
        (vec![], true),
    ] {
        let cli =
            Cli::try_parse_from(["applesauce", "compress", "."].into_iter().chain(args)).unwrap();
        let Commands::Compress(options) = cli.command else {
            panic!()
        };
        assert_eq!(options.encoder().is_ok(), valid);
        if options.backend.is_none() {
            assert_eq!(
                options.encoder().unwrap(),
                Encoder::lzfse(LzfseBackend::default())
            );
        }
    }
    #[cfg(feature = "zlib")]
    {
        let cli =
            Cli::try_parse_from(["applesauce", "compress", "-c", "zlib", "-b", "vendor", "."])
                .unwrap();
        let Commands::Compress(options) = cli.command else {
            panic!()
        };
        assert!(options
            .encoder()
            .unwrap_err()
            .contains("only valid with -c lzfse"));
    }
}

#[test]
fn scratch_arguments_and_sizes() {
    let cli = Cli::try_parse_from([
        "applesauce",
        "compress",
        "--scratch=/tmp",
        "--scratch-limit=16GiB",
        ".",
    ])
    .unwrap();
    let Commands::Compress(options) = cli.command else {
        panic!("expected compress")
    };
    assert_eq!(options.scratch_options.scratch, Some(PathBuf::from("/tmp")));
    assert_eq!(
        options.scratch_options.scratch_limit,
        Some(DEFAULT_SCRATCH_LIMIT)
    );
    assert_eq!(parse_scratch_limit("16GB").unwrap(), 16_000_000_000);
    assert_eq!(parse_scratch_limit("512MiB").unwrap(), 512 * 1024 * 1024);
    assert_eq!(parse_scratch_limit(" 1024 ").unwrap(), 1024);
    for value in ["0", "-1", "1.5GiB", "xyz", "18446744073709551615TiB"] {
        assert!(parse_scratch_limit(value).is_err(), "{value}");
    }
    assert!(Cli::try_parse_from(["applesauce", "compress", "--scratch-limit=1GiB", "."]).is_err());
    let cli = Cli::try_parse_from(["applesauce", "compress", "--scratch=/tmp", "."]).unwrap();
    let Commands::Compress(options) = cli.command else {
        panic!("expected compress")
    };
    assert_eq!(
        options
            .scratch_options
            .scratch_limit
            .unwrap_or(DEFAULT_SCRATCH_LIMIT),
        16 * 1024 * 1024 * 1024
    );
}

#[test]
fn decompress_scratch_arguments() {
    for command in ["decompress", "uncompress"] {
        let cli = Cli::try_parse_from([
            "applesauce",
            command,
            "--scratch=/tmp",
            "--scratch-limit=512MiB",
            "--manual",
            "--verify",
            ".",
        ])
        .unwrap();
        let Commands::Decompress(options) = cli.command else {
            panic!("expected decompress")
        };
        assert!(options.manual && options.verify);
        assert_eq!(options.scratch_options.scratch, Some(PathBuf::from("/tmp")));
        assert_eq!(
            options.scratch_options.scratch_limit,
            Some(512 * 1024 * 1024)
        );
        assert!(Cli::try_parse_from(["applesauce", command, "--scratch-limit=1GiB", "."]).is_err());
    }
}

#[test]
fn info_summary_arguments() {
    let cli = Cli::try_parse_from(["applesauce", "info", "--summary", "one", "two"])
        .expect("summary arguments should parse");
    let Commands::Info(info) = cli.command else {
        panic!("expected info command");
    };

    assert!(info.summary);
    assert_eq!(info.paths, [PathBuf::from("one"), PathBuf::from("two")]);
}

#[test]
fn compression_summary_requires_two_quiet_flags_to_suppress() {
    let normal = Cli::try_parse_from(["applesauce", "compress", "somefile"]).unwrap();
    assert!(normal.show_compression_summary());

    let quiet = Cli::try_parse_from(["applesauce", "compress", "-q", "somefile"]).unwrap();
    assert!(quiet.show_compression_summary());

    let silent = Cli::try_parse_from(["applesauce", "compress", "-qq", "somefile"]).unwrap();
    assert!(!silent.show_compression_summary());
}

#[test]
fn uncompress_alias() {
    let cli = Cli::try_parse_from(["applesauce", "uncompress", "somefile"])
        .expect("uncompress alias should parse");
    let Commands::Decompress(decompress) = cli.command else {
        panic!("expected decompress command");
    };
    assert_eq!(decompress.paths, [PathBuf::from("somefile")]);

    // Verify uncompress is hidden from help output
    let mut help_buf = Vec::new();
    use clap::CommandFactory;
    Cli::command().write_help(&mut help_buf).unwrap();
    let help_str = String::from_utf8(help_buf).unwrap();
    assert!(!help_str.contains("uncompress"));
}

#[test]
fn folder_info_is_aggregated() {
    let mut total = info::AfscFolderInfo::default();
    total.num_compressed_files = 2;
    total.num_files = 3;
    total.num_folders = 1;
    total.total_uncompressed_size = 1_000;
    total.total_compressed_size = 600;

    let mut additional = info::AfscFolderInfo::default();
    additional.num_compressed_files = 4;
    additional.num_files = 5;
    additional.num_folders = 2;
    additional.total_uncompressed_size = 2_000;
    additional.total_compressed_size = 800;

    add_folder_info(&mut total, &additional);

    assert_eq!(total.num_compressed_files, 6);
    assert_eq!(total.num_files, 8);
    assert_eq!(total.num_folders, 3);
    assert_eq!(total.total_uncompressed_size, 3_000);
    assert_eq!(total.total_compressed_size, 1_400);
}
