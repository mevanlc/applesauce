use applesauce::progress::{Progress, SkipReason, Task};
use indicatif::{HumanDuration, MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Initial delay to wait before checking the expected remaining time
///
/// See also [`MIN_ETA`]
const DELAY: Duration = Duration::from_millis(100);

/// Minimum expected remaining time to attach the progress bar
///
/// This is to avoid flickering when the progress bar is attached and
/// immediately finishes
const MIN_ETA: Duration = Duration::from_secs(1);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

pub struct ProgressBars {
    style: ProgressStyle,
    total_bar: ProgressBar,
    bars: MultiProgress,
    verbosity: Verbosity,
}

impl ProgressBars {
    pub fn finish(&self) {
        self.total_bar.finish_and_clear();
        let _ = self.bars.clear();
    }
}

impl ProgressBars {
    pub fn new(verbosity: Verbosity) -> Self {
        Self::with_units(verbosity, Units::Bytes)
    }

    pub fn for_info(verbosity: Verbosity) -> Self {
        Self::with_units(verbosity, Units::Files)
    }

    fn with_units(verbosity: Verbosity, units: Units) -> Self {
        Self::with_multi_progress(verbosity, units, MultiProgress::new())
    }

    fn with_multi_progress(verbosity: Verbosity, units: Units, bars: MultiProgress) -> Self {
        let smoothed_eta = |s: &ProgressState, w: &mut dyn fmt::Write| match (s.pos(), s.len()) {
            (pos, Some(len)) if pos != 0 => write!(
                w,
                "{:#}",
                HumanDuration(Duration::from_millis(
                    (s.elapsed().as_millis() * u128::from(len.saturating_sub(pos))
                        / u128::from(pos)) as u64
                ))
            )
            .unwrap(),
            _ => write!(w, "-").unwrap(),
        };
        let (total_template, file_template) = match units {
            Units::Bytes => (
                "{prefix:>25.bold} {wide_bar:.green} {bytes:>11}/{total_bytes:<11} {smoothed_eta:6}",
                "{prefix:>25.dim} {wide_bar} {bytes:>11}/{total_bytes:<11} {smoothed_eta:6}",
            ),
            Units::Files => (
                "{prefix:>25.bold} {spinner:.green} {wide_bar:.green} {pos:>8}/{len:<8} files {smoothed_eta:6}",
                "{prefix:>25.dim} {spinner} {wide_bar} {pos:>8}/{len:<8} files {smoothed_eta:6}",
            ),
        };
        #[allow(unknown_lints)] // TODO: Remove this once this clippy check is on stable
        #[allow(clippy::literal_string_with_formatting_args)]
        let total_style = ProgressStyle::with_template(total_template)
            .unwrap()
            .with_key("smoothed_eta", smoothed_eta);

        #[allow(unknown_lints)] // TODO: Remove this once this clippy check is on stable
        #[allow(clippy::literal_string_with_formatting_args)]
        let style = ProgressStyle::with_template(file_template)
            .unwrap()
            .with_key("smoothed_eta", smoothed_eta);

        let total_bar = bars
            .add(ProgressBar::new(0))
            .with_style(total_style)
            .with_prefix("Total:");
        if matches!(units, Units::Files) {
            total_bar.enable_steady_tick(DELAY);
        }

        Self {
            style,
            total_bar,
            bars,
            verbosity,
        }
    }

    pub fn prefix_len(&self) -> usize {
        // We want this to be a method, even though we don't use self
        let _ = self;
        25
    }

    pub fn multi_progress(&self) -> &MultiProgress {
        &self.bars
    }
}

#[derive(Copy, Clone)]
enum Units {
    Bytes,
    Files,
}

enum State {
    Unattached {
        bars: MultiProgress,
        first_tick: Option<Instant>,
    },
    Attached,
}

pub struct ProgressWithTotal {
    total: ProgressBar,
    single: ProgressBar,
    state: Mutex<State>,
    verbosity: Verbosity,
}

impl ProgressWithTotal {
    fn maybe_attach(&self) {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        if let State::Unattached {
            ref bars,
            ref mut first_tick,
        } = *state
        {
            let first_tick = *first_tick.get_or_insert(now);
            let pb = &self.single;
            let elapsed = now.saturating_duration_since(first_tick);
            if elapsed >= DELAY {
                let length = pb.length().unwrap_or(1);
                let remaining = length as f64 / pb.position() as f64;
                let expected_remaining = elapsed.as_secs_f64() * remaining;
                if expected_remaining > MIN_ETA.as_secs_f64() {
                    bars.insert(0, pb.clone());
                    *state = State::Attached;
                }
            }
        }
    }
}

impl Progress for ProgressBars {
    type Task = ProgressWithTotal;

    fn error(&self, path: &Path, message: &str) {
        self.total_bar
            .println(format!("{}: error: {message}", path.display()))
    }

    fn file_skipped(&self, path: &Path, why: SkipReason) {
        let required_verbosity = match why {
            SkipReason::NotFile
            | SkipReason::AlreadyCompressed
            | SkipReason::NotCompressed
            | SkipReason::EmptyFile => Verbosity::Verbose,
            SkipReason::TooLarge(_)
            | SkipReason::ReadError(_)
            | SkipReason::ZfsFilesystem
            | SkipReason::HasRequiredXattr
            | SkipReason::FsNotSupported
            | SkipReason::HardLink => Verbosity::Normal,
        };
        if self.verbosity >= required_verbosity {
            self.total_bar
                .println(format!("{}: Skipped: {why}", path.display()))
        }
    }

    fn file_task(&self, path: &Path, size: u64) -> Self::Task {
        let prefix = crate::truncate_path(path, self.prefix_len());

        let total = self.total_bar.clone();
        let single = ProgressBar::hidden()
            .with_style(self.style.clone())
            .with_prefix(prefix.to_string_lossy().into_owned());

        single.set_length(size);
        total.inc_length(size);
        ProgressWithTotal {
            total,
            single,
            state: Mutex::new(State::Unattached {
                bars: self.bars.clone(),
                first_tick: None,
            }),
            verbosity: self.verbosity,
        }
    }
}

impl Task for ProgressWithTotal {
    fn increment(&self, amt: u64) {
        self.total.inc(amt);
        self.single.inc(amt);
        self.maybe_attach();
    }

    fn error(&self, message: &str) {
        self.total.println(message);
    }

    fn not_compressible_enough(&self, path: &Path) {
        if self.verbosity >= Verbosity::Verbose {
            let message = format!("{}: Not compressible enough, file grew", path.display());
            self.total.println(message);
        }
    }
}

pub struct ProgressBarWriter<W> {
    multi_progress: MultiProgress,
    inner: W,
}

impl<W> ProgressBarWriter<W> {
    pub fn new(multi_progress: MultiProgress, inner: W) -> Self {
        Self {
            multi_progress,
            inner,
        }
    }
}

impl<W: Write> Write for ProgressBarWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.multi_progress.suspend(|| self.inner.write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.multi_progress.suspend(|| self.inner.flush())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::{ProgressDrawTarget, TermLike};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Debug, Default)]
    struct TestTerm {
        line_visible: Arc<AtomicBool>,
    }

    impl TestTerm {
        fn line_visible(&self) -> bool {
            self.line_visible.load(Ordering::Relaxed)
        }
    }

    impl TermLike for TestTerm {
        fn width(&self) -> u16 {
            120
        }

        fn move_cursor_up(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> std::io::Result<()> {
            Ok(())
        }

        fn write_line(&self, s: &str) -> std::io::Result<()> {
            self.line_visible.store(!s.is_empty(), Ordering::Relaxed);
            Ok(())
        }

        fn write_str(&self, s: &str) -> std::io::Result<()> {
            if !s.is_empty() {
                self.line_visible.store(true, Ordering::Relaxed);
            }
            Ok(())
        }

        fn clear_line(&self) -> std::io::Result<()> {
            self.line_visible.store(false, Ordering::Relaxed);
            Ok(())
        }

        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn finish_clears_rendered_progress_bars() {
        let term = TestTerm::default();
        let bars =
            MultiProgress::with_draw_target(ProgressDrawTarget::term_like(Box::new(term.clone())));
        let progress = ProgressBars::with_multi_progress(Verbosity::Normal, Units::Bytes, bars);
        let file_bar = progress.bars.add(ProgressBar::new(1));

        progress.total_bar.tick();
        file_bar.tick();
        assert!(term.line_visible());

        progress.finish();
        assert!(!term.line_visible());
    }
}
