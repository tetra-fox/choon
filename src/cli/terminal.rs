//! what a check shows in a terminal: a bar per file under way over one for them all with what
//! was found so far, then one file's reading in detail or the list and the counts of many

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anstyle::{AnsiColor, Style};
use choon::grid::Evidence;
use choon::{OpusUse, Reading, Verdict};
use comfy_table::{CellAlignment, ContentArrangement, LineStyle, Table, TableStyle};
use console::{Alignment, Term};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::{Checked, NOT_READ, encoder, flag_words, flagged, khz, label, said};

/// the verdicts in the order the summary counts them
const VERDICTS: [Verdict; 5] = [
  Verdict::Consistent,
  Verdict::LikelyTranscode,
  Verdict::MixedSources,
  Verdict::PossiblyLossy,
  Verdict::Inconclusive,
];

/// a file and what came of reading it: what it measured, or why it was not read
pub struct Row<'a> {
  pub path: PathBuf,
  /// the directory named that a walk found the file under; `None` for a file named itself
  pub root: Option<&'a Path>,
  pub outcome: Result<Box<Checked>, String>,
}

/// what a file that ended adds to the counts beside the bar
pub enum Counted {
  /// a reading flagged with this verdict
  Flagged(Verdict),
  NotRead,
}

/// what the bar counts, in the order it says them
const COUNTED: [(Option<Verdict>, &str); 4] = [
  (Some(Verdict::LikelyTranscode), "likely transcode"),
  (Some(Verdict::MixedSources), "mixed sources"),
  (Some(Verdict::PossiblyLossy), "possibly lossy"),
  (None, NOT_READ),
];

/// `text` in the colour of `verdict`, or of a file not read without one, when `colour`
pub fn painted(verdict: Option<Verdict>, text: &str, colour: bool) -> String {
  let tint = match verdict {
    Some(Verdict::LikelyTranscode | Verdict::MixedSources) => Some(AnsiColor::Red),
    Some(Verdict::PossiblyLossy) => Some(AnsiColor::Yellow),
    Some(Verdict::Consistent) => Some(AnsiColor::Green),
    Some(Verdict::Inconclusive) => None,
    None => Some(AnsiColor::Magenta),
  };
  match tint.filter(|_| colour) {
    Some(tint) => {
      let style = Style::new().fg_color(Some(tint.into()));
      format!("{style}{text}{style:#}")
    }
    None => text.to_string(),
  }
}

/// `path` as a terminal shows it. a control character in a file's name is written as its
/// escape, so a name cannot move the cursor, colour what follows it or start a line
pub fn legible(path: &Path) -> String {
  let mut text = String::new();
  for character in path.to_string_lossy().chars() {
    if character.is_control() {
      text.extend(character.escape_default());
    } else {
      text.push(character);
    }
  }
  text
}

/// a file under way
struct Flying {
  file: usize,
  name: String,
  /// how far each measuring of it says it is, in thousandths
  progress: Vec<Arc<AtomicU32>>,
  /// its bar, when the terminal has a line for one
  bar: Option<ProgressBar>,
}

/// the bars of a check: one per file under way, as many as fit, a line counting the rest, and
/// under them one bar for all the files with the count of what was found
pub struct Bars {
  multi: MultiProgress,
  term: Term,
  /// `None` for a single file
  overall: Option<ProgressBar>,
  /// how many files there are, where that is known before they are read
  total: Option<usize>,
  /// how many have ended
  done: usize,
  /// how many ended as each of [`COUNTED`]
  counts: [usize; COUNTED.len()],
  started: Instant,
  /// the line counting the files under way that have no bar
  more: Option<ProgressBar>,
  /// in the order they started
  flying: Vec<Flying>,
}

/// how wide a file's bar is drawn, in columns
const BAR: usize = 20;
/// the columns the overall bar keeps beside its count before the count says less: its
/// brackets, a space and ten columns of bar
const MIN_BAR: usize = 13;
/// the columns a file's line holds beside its name: a space, the bar in brackets, a space and
/// the percentage
const BESIDE_NAME: usize = BAR + 8;

fn style(template: &str) -> ProgressStyle {
  ProgressStyle::with_template(template)
    .expect("the template is well formed")
    .progress_chars("=> ")
}

impl Bars {
  /// the bars of a check of `total` files, or of one that learns how many there are as it
  /// reads them
  pub fn new(total: Option<usize>) -> Self {
    let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stdout());
    let overall = (total != Some(1)).then(|| {
      let overall = match total {
        Some(total) => ProgressBar::new(total as u64).with_style(style("[{wide_bar}] {msg}")),
        None => ProgressBar::no_length().with_style(style("{msg}")),
      };
      multi.add(overall)
    });
    let bars = Self {
      multi,
      term: Term::stdout(),
      overall,
      total,
      done: 0,
      counts: [0; COUNTED.len()],
      started: Instant::now(),
      more: None,
      flying: Vec::new(),
    };
    // a bar is drawn from its first change on, which without this is the first file's end
    bars.count();
    bars
  }

  pub fn started(&mut self, file: usize, name: String, progress: Vec<Arc<AtomicU32>>) {
    self.flying.push(Flying {
      file,
      name,
      progress,
      bar: None,
    });
  }

  pub fn ended(&mut self, file: usize, counted: Option<Counted>) {
    if let Some(at) = self.flying.iter().position(|flying| flying.file == file)
      && let Some(bar) = self.flying.remove(at).bar
    {
      remove(&self.multi, &bar);
    }
    self.done += 1;
    let counted = counted.map(|counted| match counted {
      Counted::Flagged(verdict) => Some(verdict),
      Counted::NotRead => None,
    });
    if let Some(at) = COUNTED.iter().position(|(kind, _)| Some(*kind) == counted) {
      self.counts[at] += 1;
    }
    self.count();
  }

  /// writes beside the overall bar how many files have ended, how many of them ended as each
  /// of [`COUNTED`], and about how long the others take, where the line has the room
  fn count(&self) {
    let Some(overall) = &self.overall else {
      return;
    };
    let mut said = match self.total {
      Some(total) => format!("{} of {total}", self.done),
      None => format!("{} read", self.done),
    };
    for ((_, words), count) in COUNTED.iter().zip(self.counts) {
      if count > 0 {
        said += &format!(", {count} {words}");
      }
    }
    if let Some(total) = self.total.filter(|_| self.done > 0) {
      // the rate over the whole run so far: files end in bursts, which a rate over the last
      // few would follow
      let per_second = self.done as f64 / self.started.elapsed().as_secs_f64();
      let left = Duration::from_secs_f64((total - self.done) as f64 / per_second);
      let timed = format!("{said}, ETA {}", length(left));
      if timed.len() + MIN_BAR <= usize::from(self.term.size().1) {
        said = timed;
      }
    }
    overall.set_position(self.done as u64);
    overall.set_message(said);
  }

  /// brings the bars up to date with the files under way and the terminal's size
  pub fn tick(&mut self) -> io::Result<()> {
    let Self {
      multi,
      term,
      overall,
      more,
      flying,
      ..
    } = self;
    let (multi, overall) = (&*multi, &*overall);
    // under the files' bars and over the one for the run
    let add = |bar: ProgressBar| match overall {
      Some(overall) => multi.insert_before(overall, bar),
      None => multi.add(bar),
    };
    let (rows, columns) = term.size();
    let room = room(rows, overall.is_some(), flying.len());
    let without = flying.len().saturating_sub(room);
    match (&*more, without) {
      (Some(counted), 0) => {
        remove(multi, counted);
        *more = None;
      }
      (Some(counted), _) => counted.set_message(format!("+{without} more in flight")),
      (None, 0) => {}
      (None, _) => {
        let counted = add(
          ProgressBar::no_length()
            .with_style(style("{msg}"))
            .with_message(format!("+{without} more in flight")),
        );
        // a bar is drawn from its first change on
        counted.tick();
        *more = Some(counted);
      }
    }
    let name_width = usize::from(columns)
      .saturating_sub(BESIDE_NAME)
      .clamp(8, 48);
    for (at, flying) in flying.iter_mut().enumerate() {
      if at >= room {
        if let Some(bar) = flying.bar.take() {
          remove(multi, &bar);
        }
        continue;
      }
      let bar = flying.bar.get_or_insert_with(|| {
        let bar = ProgressBar::new(1000).with_style(style(&format!(
          "{{prefix}} [{{bar:{BAR}}}] {{percent:>3}}%"
        )));
        match &*more {
          Some(counted) => multi.insert_before(counted, bar),
          None => add(bar),
        }
      });
      let name = console::truncate_str(&flying.name, name_width, "...");
      bar.set_prefix(console::pad_str(&name, name_width, Alignment::Left, None).into_owned());
      let measured: u64 = flying
        .progress
        .iter()
        .map(|measuring| u64::from(measuring.load(Ordering::Relaxed)))
        .sum();
      bar.set_position(measured / flying.progress.len().max(1) as u64);
    }
    // indicatif 0.18.6 pads every line it draws to the terminal's width and leaves the cursor
    // at the right edge of the last one (draw_target.rs, `draw_to_term`). a terminal writes
    // `^C` there at an interrupt, on a line of its own, and the bars are then cleared one line
    // too low, which leaves the first on the screen. at the start of the line `^C` is written
    // over the bar, which the next draw or the clearing takes off
    // TODO: drop once indicatif clears its bars wherever the cursor was moved to
    term.write_str("\r")
  }

  /// prints `line` on stderr above the bars
  pub fn complain(&self, line: &str) {
    self.multi.suspend(|| eprintln!("{line}"));
  }

  /// takes every bar off the screen
  pub fn finish(self) {
    let bars = self.flying.into_iter().filter_map(|flying| flying.bar);
    for bar in bars.chain(self.more).chain(self.overall) {
      remove(&self.multi, &bar);
    }
  }
}

/// takes `bar` off the screen
fn remove(multi: &MultiProgress, bar: &ProgressBar) {
  bar.finish_and_clear();
  multi.remove(bar);
}

/// how many of `flying` files get a bar in a terminal of `rows` lines. the bars, the line
/// counting the files without one and the overall bar take at most half the lines
fn room(rows: u16, overall: bool, flying: usize) -> usize {
  let lines = usize::from(rows / 2)
    .saturating_sub(usize::from(overall))
    .max(1);
  if flying > lines {
    lines.saturating_sub(1).max(1)
  } else {
    lines
  }
}

/// a table without borders, `width` columns wide at most, its header underlined when it has
/// one
fn table(width: u16) -> Table {
  let mut table = Table::new();
  table
    .load_style(TableStyle::new().header_separator(LineStyle::none().fill('-').junction(' ')))
    .set_content_arrangement(ContentArrangement::Dynamic)
    .set_width(width);
  table
}

/// the lines of `table` indented by `indent` spaces, two spaces between its columns and none
/// after its last
fn lines(table: &mut Table, indent: u16) -> String {
  for (at, column) in table.column_iter_mut().enumerate() {
    column.set_padding((if at == 0 { indent } else { 0 }, 2));
  }
  table.lines().fold(String::new(), |mut text, line| {
    text += line.trim_end();
    text += "\n";
    text
  })
}

/// a length of time in words: seconds, to a tenth under ten of them, and minutes and seconds
/// from a minute on
fn length(time: Duration) -> String {
  match time.as_secs() {
    0..10 => format!("{:.1} s", time.as_secs_f64()),
    seconds @ 10..60 => format!("{seconds} s"),
    seconds => format!("{} min {} s", seconds / 60, seconds % 60),
  }
}

/// one file's reading in detail, under each profile when `profiled`: aligned lines of a label
/// and its value, wrapped at `width` columns, with the reading's summary when `verbose`
pub fn readout(
  path: &Path,
  checked: &Checked,
  profiled: bool,
  verbose: bool,
  colour: bool,
  width: u16,
) -> String {
  let claimed = &checked.claimed;
  let mut format = vec![claimed.codec.clone(), format!("{} Hz", claimed.sample_rate)];
  if let Some(bits) = claimed.bit_depth {
    format.push(format!("{bits} bit"));
  }
  format.push(match claimed.channels {
    1 => "1 channel".to_string(),
    channels => format!("{channels} channels"),
  });
  if let Some(kbps) = checked.bitrate_kbps {
    format.push(format!("{kbps} kbit/s"));
  }
  let seconds = checked.duration.round() as u64;
  format.push(format!("{}:{:02}", seconds / 60, seconds % 60));
  let format = format.join(", ");

  let mut text = String::new();
  for read in &checked.readings {
    let reading = &read.reading;
    text += &legible(path);
    if profiled {
      text += &format!(" [{}]", read.profile);
    }
    text += "\n";
    let mut table = table(width);
    let mut add = |label: &str, value: String| {
      table.add_row([label, &value]);
    };
    add("Format", format.clone());
    if let Some(coding) = OpusUse::describe(&checked.opus) {
      add("Opus packets", coding);
    }
    if let Some((rate, multiple)) = checked.dsd_rate.zip(checked.dsd_multiple) {
      add(
        "DSD",
        format!("DSD{multiple}, one-bit samples at {rate} Hz"),
      );
    }
    if let Some(he_aac) = checked.he_aac {
      add(
        "AAC profile",
        format!("{he_aac}, core at {} Hz", he_aac.core_rate),
      );
    }
    add(
      "Verdict",
      painted(Some(reading.verdict), label(reading.verdict), colour),
    );
    add("Grid", grid(checked, reading));
    add(
      "Cutoff",
      reading
        .cutoff_hz
        .map_or_else(|| "none found".to_string(), khz),
    );
    if let Some(encoder) = encoder(reading) {
      add("Encoder", encoder);
    }
    if reading.verdict != Verdict::Inconclusive {
      add("Confidence", format!("{:.2}", reading.confidence));
    }
    if !reading.flags.is_empty() {
      let flags: Vec<&str> = reading.flags.iter().copied().map(flag_words).collect();
      add("Flags", flags.join(", "));
    }
    if verbose {
      add("Summary", reading.summary.clone());
    }
    if !reading.limitations.is_empty() {
      let limitations: Vec<String> = reading
        .limitations
        .iter()
        .map(ToString::to_string)
        .collect();
      add("Limitations", limitations.join("\n"));
    }
    text += &lines(&mut table, 2);
    text += "\n";
  }
  text
}

/// what the grid scan showed of a file, for its reading
fn grid(checked: &Checked, reading: &Reading) -> String {
  match (&reading.grid, &checked.grid) {
    (Some(found), _) => match &found.evidence {
      Evidence::Count(scan) => {
        let quantizer = found.quantizer.as_ref().map_or_else(String::new, |steps| {
          format!(", {} bands on whole quantizer steps", steps.whole)
        });
        format!(
          "{} at offset {}, z {:.0}, {} of {} stretches agree{quantizer}",
          found.codec,
          scan.offset,
          scan.z,
          scan.segments_agreeing,
          scan.segment_z.len()
        )
      }
      Evidence::Part { segments, scan } => format!(
        "{} at offset {}, z {:.0}, in {segments} of {} stretches",
        found.codec,
        scan.offset,
        scan.z,
        scan.segment_z.len()
      ),
      Evidence::CountAt { rate, scan } => format!(
        "{} at offset {} once at {} kHz, z {:.0}, {} of {} stretches agree",
        found.codec,
        scan.offset,
        f64::from(*rate) / 1000.0,
        scan.z,
        scan.segments_agreeing,
        scan.segment_z.len()
      ),
      Evidence::Steps { offset, steps } => format!(
        "{} at offset {offset}, {} of {} bands on whole quantizer steps",
        found.codec, steps.whole, steps.tested
      ),
      Evidence::AacSteps { offset, steps } => format!(
        "{} at offset {offset}, {} of {} bands on whole quantizer steps",
        found.codec, steps.whole, steps.tested
      ),
      Evidence::Lattice(scan) => format!(
        "{} at offset {}, {} frames on its energy lattice in the stretches, {} in the windows",
        found.codec, scan.offset, scan.frames, scan.uncut_frames
      ),
      Evidence::Shifted { shifted } => format!(
        "{} at offset {}, {} frames on its energy lattice at another level in the stretches, \
         {} in the windows",
        found.codec, shifted.offset, shifted.frames, shifted.uncut_frames
      ),
      Evidence::Unfiltered { unfiltered } => format!(
        "{} at offset {}, {} of {} frames on its energy lattice under its pitch filter in the \
         windows",
        found.codec,
        found.evidence.offset(),
        unfiltered.frames,
        unfiltered.searched
      ),
      Evidence::Hybrid { hybrid } => format!(
        "{} at offset {}, {} of {} hybrid frames on its energy lattice in the stretches, {} of \
         {} in the windows",
        found.codec,
        hybrid.offset,
        hybrid.frames,
        hybrid.tested,
        hybrid.uncut_frames,
        hybrid.uncut_tested
      ),
      Evidence::Short { short } => format!(
        "{} at offset {}, {} frames of {} ms on its energy lattice in the stretches, {} in the \
         windows",
        found.codec,
        short.offset,
        short.frames,
        short.milliseconds(),
        short.uncut_frames
      ),
    },
    (None, Some(scan)) => {
      let opus = scan.opus.as_ref().map_or_else(String::new, |opus| {
        format!(
          ", Opus frames {} in the stretches, {} in the windows",
          opus.frames, opus.uncut_frames
        )
      });
      // AAC's grid under whichever of its windows shows it more
      let aac = scan
        .aac_sine
        .as_ref()
        .map_or(scan.aac.z, |sine| sine.z.max(scan.aac.z));
      format!(
        "none over the thresholds (MP3 z {:.1}, AAC z {aac:.1}{opus})",
        scan.mp3.z
      )
    }
    (None, None) => "not scanned".to_string(),
  }
}

/// a terminal narrower than this has no room for the table of files, and gets them as the
/// lines a pipe gets
const TABLE_MIN_WIDTH: u16 = 60;
/// the columns a file's name keeps in the table, however little room the other columns leave
const NAME_MIN: usize = 12;
/// the spaces before a file's line under its directory
const INDENT: usize = 2;
/// the spaces between two columns
const GAP: usize = 2;

/// what a check of many files found: the flagged files, or every file when `verbose`, by
/// directory, then the files not read, each flagged file in detail when `verbose`, and last
/// the count of each verdict, under each profile when `profiled`, with the files read and not
/// read, how long it took and what an interrupt `left`
pub fn tables(
  rows: &[Row<'_>],
  profiled: bool,
  verbose: bool,
  took: Duration,
  left: Option<&str>,
  colour: bool,
  width: u16,
) -> String {
  let read: Vec<(&Path, &Checked)> = rows
    .iter()
    .filter_map(|row| Some((&*row.path, &**row.outcome.as_ref().ok()?)))
    .collect();
  let not_read: Vec<(&Path, &str)> = rows
    .iter()
    .filter_map(|row| Some((&*row.path, row.outcome.as_ref().err()?.as_str())))
    .collect();
  let found: Vec<(&Path, &Checked)> = read
    .iter()
    .copied()
    .filter(|(_, checked)| checked.readings.iter().any(|r| flagged(r.reading.verdict)))
    .collect();
  let mut sections = Vec::new();

  if width < TABLE_MIN_WIDTH {
    let mut lines = String::new();
    for row in rows {
      let path = legible(&row.path);
      match &row.outcome {
        Ok(checked) => {
          if verbose || checked.readings.iter().any(|r| flagged(r.reading.verdict)) {
            lines += &format!("{path}: {}\n", said(checked, colour));
          }
        }
        Err(why) => lines += &format!("{path}: {} ({why})\n", painted(None, NOT_READ, colour)),
      }
    }
    sections.push(lines);
  } else {
    sections.push(listing(rows, profiled, verbose, colour, usize::from(width)));
    let mut unread = String::new();
    if !not_read.is_empty() {
      unread += "Not read\n--------\n";
      for (path, why) in &not_read {
        unread += &format!("{}: {why}\n", legible(path));
      }
    }
    sections.push(unread);
  }

  let mut detail = String::new();
  if verbose {
    for (path, checked) in &found {
      detail += &readout(path, checked, profiled, true, colour, width);
    }
  }
  sections.push(detail.trim_end().to_string() + "\n");

  // every file is read under the same profiles
  let profiles: Vec<&str> = read.first().map_or_else(Vec::new, |(_, checked)| {
    checked
      .readings
      .iter()
      .map(|read| read.profile.as_str())
      .collect()
  });
  if !profiles.is_empty() {
    let mut counts = table(width);
    let mut header = vec!["Verdict"];
    if profiled {
      header.extend(&profiles);
    } else {
      header.push("Files");
    }
    counts.set_header(header);
    for verdict in VERDICTS {
      let mut row = vec![painted(Some(verdict), label(verdict), colour)];
      row.extend((0..profiles.len()).map(|profile| {
        read
          .iter()
          .filter(|(_, checked)| checked.readings[profile].reading.verdict == verdict)
          .count()
          .to_string()
      }));
      counts.add_row(row);
    }
    right_align(&mut counts, 1);
    sections.push(lines(&mut counts, 0));
  }

  let mut totals = table(width);
  totals.add_row(["Files read", &read.len().to_string()]);
  totals.add_row(["Files not read", &not_read.len().to_string()]);
  totals.add_row(["Wall time", &length(took)]);
  totals.add_row([
    "Files per minute",
    &format!("{:.0}", rows.len() as f64 / took.as_secs_f64() * 60.0),
  ]);
  if let Some(left) = left {
    totals.add_row(["Interrupted", left]);
  }
  right_align(&mut totals, 1);
  sections.push(lines(&mut totals, 0));

  sections.retain(|section| !section.trim().is_empty());
  sections.join("\n")
}

/// a file in the list, with the name it has under its directory
type Listed<'a> = (&'a Row<'a>, String);

/// a line of the list of files
enum Entry {
  /// a directory, over its files
  Heading(String),
  /// a directory whose files all read alike, and what they read as
  Alike(String, String),
  /// a reading of a file: its cells, the file's name last
  File(Vec<String>),
}

/// the files a check lists, by directory in the order of their paths. a directory is a
/// heading over its files, each a line of a table under its name, and a directory whose files
/// all read alike is one line. a directory named comes once, as it was named, over the
/// directories under it, which are named from there. a file named itself is under its
/// directory as named, or on a line of its own by its path, ahead of the directories, when
/// no other file named shares the directory. empty when no file is listed
fn listing(rows: &[Row<'_>], profiled: bool, verbose: bool, colour: bool, width: usize) -> String {
  let none = Path::new("");
  // how many of the files named themselves each directory holds
  let mut named: HashMap<&Path, usize> = HashMap::new();
  for row in rows.iter().filter(|row| row.root.is_none()) {
    *named.entry(row.path.parent().unwrap_or(none)).or_default() += 1;
  }
  let name = |path: &Path| legible(Path::new(path.file_name().unwrap_or(path.as_os_str())));
  // every file of a directory, by the directory named it is under and its own from there
  let mut groups: BTreeMap<(Option<&Path>, &Path), Vec<Listed<'_>>> = BTreeMap::new();
  for row in rows {
    let (directory, name) = match row.root {
      Some(root) => {
        // a walk's paths start with the directory it was given
        let within = row.path.strip_prefix(root).unwrap_or(&row.path);
        (within.parent().unwrap_or(none), name(within))
      }
      None => match row.path.parent() {
        Some(parent) if parent != none && named[parent] > 1 => (parent, name(&row.path)),
        _ => (none, legible(&row.path)),
      },
    };
    groups
      .entry((row.root, directory))
      .or_default()
      .push((row, name));
  }

  let mut entries = Vec::new();
  let mut under = None;
  for ((root, directory), files) in &groups {
    let heading = match (root, *directory == none) {
      (None, true) => None,
      (Some(root), true) => Some(heading(root)),
      (_, false) => Some(heading(directory)),
    };
    let alike = heading
      .as_ref()
      .filter(|_| !verbose && !profiled)
      .and_then(|_| alike(files, colour));
    let lines: Vec<Entry> = files
      .iter()
      .flat_map(|(row, name)| cells(row, name, profiled, verbose, colour))
      .map(Entry::File)
      .collect();
    if alike.is_none() && lines.is_empty() {
      continue;
    }
    if let Some(root) = root
      && under.replace(*root) != Some(*root)
      && *directory != none
    {
      entries.push(Entry::Heading(self::heading(root)));
    }
    match (heading, alike) {
      (Some(heading), Some(alike)) => entries.push(Entry::Alike(heading, alike)),
      (heading, _) => {
        entries.extend(heading.map(Entry::Heading));
        entries.extend(lines);
      }
    }
  }

  let mut header = vec!["Verdict", "Grid", "Cutoff", "Encoder"];
  if profiled {
    header.push("Profile");
  }
  let (file, columns) = ("File", header.len());
  // the columns before the name are as wide as their widest cell
  let mut widths: Vec<usize> = header.iter().map(|title| title.len()).collect();
  let mut longest = file.len();
  let mut listed = false;
  for entry in &entries {
    if let Entry::File(cells) = entry {
      listed = true;
      for (width, cell) in widths.iter_mut().zip(cells) {
        *width = (*width).max(console::measure_text_width(cell));
      }
      longest = longest.max(console::measure_text_width(&cells[columns]));
    }
  }
  let before: usize = INDENT + widths.iter().map(|width| width + GAP).sum::<usize>();
  let name_width = longest.min(width.saturating_sub(before).max(NAME_MIN));
  let line = |cells: &mut dyn Iterator<Item = &str>, name: &str| {
    let mut line = " ".repeat(INDENT);
    for (cell, width) in cells.zip(&widths) {
      line += &console::pad_str(cell, width + GAP, Alignment::Left, None);
    }
    line + &cut(name, name_width) + "\n"
  };

  let mut text = String::new();
  if listed {
    text += &line(&mut header.iter().copied(), file);
    let rules: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    text += &line(
      &mut rules.iter().map(String::as_str),
      &"-".repeat(name_width),
    );
  }
  for entry in &entries {
    match entry {
      Entry::Heading(directory) => {
        text += &cut(directory, width);
        text += "\n";
      }
      Entry::Alike(directory, alike) => {
        let room = width.saturating_sub(GAP + console::measure_text_width(alike));
        text += &format!("{}  {alike}\n", cut(directory, room.max(NAME_MIN)));
      }
      Entry::File(cells) => {
        text += &line(
          &mut cells[..columns].iter().map(String::as_str),
          &cells[columns],
        );
      }
    }
  }
  text
}

/// `directory` as a heading, with the separator a directory's path ends in
fn heading(directory: &Path) -> String {
  let mut heading = legible(directory);
  if !heading.ends_with(['/', std::path::MAIN_SEPARATOR]) {
    heading.push(std::path::MAIN_SEPARATOR);
  }
  heading
}

/// the lines of a file in the list: one for each of its readings that is flagged, or for
/// each reading when `verbose`, which also states a grid's offset. none for a file not read
fn cells(
  row: &Row<'_>,
  name: &str,
  profiled: bool,
  verbose: bool,
  colour: bool,
) -> Vec<Vec<String>> {
  let Ok(checked) = &row.outcome else {
    return Vec::new();
  };
  let readings = checked.readings.iter();
  readings
    .filter(|read| verbose || flagged(read.reading.verdict))
    .map(|read| {
      let reading = &read.reading;
      let grid = reading.grid.as_ref().map_or_else(String::new, |found| {
        if verbose {
          format!("{}, offset {}", found.codec, found.evidence.offset())
        } else {
          found.codec.to_string()
        }
      });
      let mut cells = vec![
        painted(Some(reading.verdict), label(reading.verdict), colour),
        grid,
        reading.cutoff_hz.map_or_else(String::new, khz),
        encoder(reading).unwrap_or_default(),
      ];
      if profiled {
        cells.push(read.profile.clone());
      }
      cells.push(name.to_string());
      cells
    })
    .collect()
}

/// what the files of a directory all read as, when there are several, each has one reading,
/// and the readings are flagged with one verdict, one grid's codec and one encoder: how many
/// they are, the verdict, and in parentheses the grid, their cutoffs from the lowest to the
/// highest, and the encoder
fn alike(files: &[Listed<'_>], colour: bool) -> Option<String> {
  let readings: Vec<&Reading> = files
    .iter()
    .map(|(row, _)| match &row.outcome.as_ref().ok()?.readings[..] {
      [read] => Some(&read.reading),
      _ => None,
    })
    .collect::<Option<_>>()?;
  let [first, rest @ ..] = &readings[..] else {
    return None;
  };
  let read_as = |reading: &Reading| {
    let codec = reading.grid.as_ref().map(|found| found.codec);
    (reading.verdict, codec, encoder(reading))
  };
  if rest.is_empty()
    || !flagged(first.verdict)
    || rest.iter().any(|other| read_as(other) != read_as(first))
  {
    return None;
  }
  let mut found = Vec::new();
  if let Some(grid) = &first.grid {
    found.push(format!("{} grid", grid.codec));
  }
  let cutoffs = readings.iter().filter_map(|reading| reading.cutoff_hz);
  let range = cutoffs.fold(None, |range: Option<(f64, f64)>, hz| {
    let (low, high) = range.unwrap_or((hz, hz));
    Some((low.min(hz), high.max(hz)))
  });
  if let Some((low, high)) = range {
    let (low, high) = (khz(low), khz(high));
    found.push(match low.strip_suffix(" kHz") {
      Some(low_khz) if low != high => format!("cutoff {low_khz} to {high}"),
      _ => format!("cutoff {high}"),
    });
  }
  found.extend(encoder(first));
  let count = readings.len();
  let verdict = painted(Some(first.verdict), label(first.verdict), colour);
  Some(if found.is_empty() {
    format!("{count} of {count} {verdict}")
  } else {
    format!("{count} of {count} {verdict} ({})", found.join(", "))
  })
}

/// `text` in at most `width` columns, with `...` in the place of its middle when it is wider
fn cut(text: &str, width: usize) -> Cow<'_, str> {
  if console::measure_text_width(text) <= width {
    return Cow::Borrowed(text);
  }
  let kept = width.saturating_sub(3);
  let columns = |character: char| console::measure_text_width(character.encode_utf8(&mut [0; 4]));
  // the characters that fit in `room` columns, from the start or from the end
  let fitting = |characters: &mut dyn Iterator<Item = (usize, char)>, mut room: usize| {
    let mut last = None;
    for (at, character) in characters {
      let Some(left) = room.checked_sub(columns(character)) else {
        break;
      };
      (room, last) = (left, Some((at, character)));
    }
    last
  };
  let head = fitting(&mut text.char_indices(), kept.div_ceil(2))
    .map_or(0, |(at, character)| at + character.len_utf8());
  let tail = fitting(&mut text.char_indices().rev(), kept / 2).map_or(text.len(), |(at, _)| at);
  Cow::Owned(format!("{}...{}", &text[..head], &text[tail.max(head)..]))
}

/// right-aligns the columns of `table` from column `first` on
fn right_align(table: &mut Table, first: usize) {
  for column in table.column_iter_mut().skip(first) {
    column.set_cell_alignment(CellAlignment::Right);
  }
}

#[cfg(test)]
mod tests {
  use std::path::Path;
  use std::sync::Arc;
  use std::time::Duration;

  use choon::fixtures::{self, Bitrate, music, pink};
  use choon::grid::{Codec, Family};
  use choon::{CancelCheck, Coverage};

  use choon::Verdict;

  use super::{Row, cut, legible, readout, room, tables};
  use crate::{Checked, Thresholds, analyzed, encoder, khz};

  /// what the shipped thresholds make of an honest FLAC and of one made from a 128 kbps MP3
  fn honest_and_fake() -> (Box<Checked>, Box<Checked>) {
    let dir = tempfile::tempdir().unwrap();
    let honest = dir.path().join("honest.flac");
    fixtures::write_flac(&music(1, 44_100, pink), 16, false, &honest);
    let fake = dir.path().join("fake.flac");
    let through = fixtures::through_mp3(&music(1, 44_100, pink), Bitrate::Kbps128, dir.path());
    fixtures::write_flac(&through, 16, false, &fake);
    let profiles = [("default".to_string(), Thresholds::default())];
    let never: CancelCheck = Arc::new(|| false);
    let checked = |path: &Path| {
      let progress = [Arc::default()];
      let (checked, _) = analyzed(
        path,
        &profiles,
        Coverage::Full,
        None,
        false,
        &progress,
        &never,
      )
      .unwrap();
      Box::new(checked)
    };
    (checked(&honest), checked(&fake))
  }

  /// `text` without the codes that colour it
  fn uncoloured(text: &str) -> String {
    let mut plain = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("\u{1b}[") {
      plain += &rest[..at];
      let code = &rest[at..];
      rest = &code[code.find('m').unwrap() + 1..];
    }
    plain + rest
  }

  #[test]
  fn an_encoder_is_the_grids_codec_its_family_and_the_cutoffs_setting() {
    let (_, fake) = honest_and_fake();
    let reading = &fake.readings[0].reading;
    let named = |codec, family| {
      let mut reading = reading.clone();
      let grid = reading.grid.as_mut().expect("the fake carries a grid");
      (grid.codec, grid.family) = (codec, family);
      encoder(&reading)
    };
    let mp3 = |family| named(Codec::Mp3, family);
    assert_eq!(mp3(None).as_deref(), Some("MP3 ~128 kbps"));
    assert_eq!(
      mp3(Some(Family::Untagged)).as_deref(),
      Some("MP3 ~128 kbps")
    );
    assert_eq!(
      mp3(Some(Family::Lame)).as_deref(),
      Some("MP3 (LAME) ~128 kbps")
    );
    // the profiles are LAME's cutoffs, so another encoder is named without a setting
    assert_eq!(mp3(Some(Family::Helix)).as_deref(), Some("MP3 (Helix)"));
    // a profile named for another codec than the grid's names no setting
    assert_eq!(
      named(Codec::Aac, Some(Family::Apple)).as_deref(),
      Some("AAC (Apple)")
    );
    assert_eq!(named(Codec::Aac, None), None);
    // a cutoff two codecs share names the other one only where no grid says which it was
    let shared = |grid: bool| {
      let mut reading = reading.clone();
      let guess = reading
        .encoder_guess
        .as_mut()
        .expect("the fake has a guess");
      guess.profile.setting = "~320 kbps".to_string();
      guess.profile.also = Some("AAC".to_string());
      if !grid {
        reading.grid = None;
      }
      encoder(&reading)
    };
    assert_eq!(shared(true).as_deref(), Some("MP3 ~320 kbps"));
    assert_eq!(shared(false).as_deref(), Some("MP3 ~320 kbps or AAC"));
  }

  #[test]
  fn a_control_character_in_a_name_is_written_as_its_escape() {
    assert_eq!(
      legible(Path::new("a\nb\u{1b}[31m \u{e9}.flac")),
      "a\\nb\\u{1b}[31m \u{e9}.flac"
    );
  }

  #[test]
  fn a_readout_lines_up_its_values_and_wraps_them_under_themselves() {
    let (_, fake) = honest_and_fake();
    let text = readout(Path::new("album/fake.flac"), &fake, false, true, false, 60);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "album/fake.flac");
    assert!(
      lines.contains(&"  Verdict       likely transcode"),
      "{text}"
    );
    assert!(lines.contains(&"  Encoder       MP3 ~128 kbps"), "{text}");
    assert!(
      lines
        .iter()
        .any(|line| line.starts_with("  Grid          MP3 at offset ")),
      "{text}"
    );
    // the summary is longer than a line: what follows its first line starts under its value
    let summary = lines
      .iter()
      .position(|line| line.starts_with("  Summary"))
      .unwrap();
    assert!(lines[summary + 1].starts_with("                "), "{text}");
    assert!(lines.iter().all(|line| line.len() <= 60), "{text}");

    let coloured = readout(Path::new("album/fake.flac"), &fake, false, true, true, 60);
    assert!(
      coloured.contains("\u{1b}[31mlikely transcode\u{1b}[0m"),
      "{coloured}"
    );
    assert_eq!(uncoloured(&coloured), text);
  }

  /// a row of a file a walk of `root` found, or of one named itself
  fn row<'a>(path: &str, root: Option<&'a str>, checked: &Checked) -> Row<'a> {
    Row {
      path: path.into(),
      root: root.map(Path::new),
      outcome: Ok(Box::new(checked.clone())),
    }
  }

  /// `tables` of `rows` without colour: the lines of the list, which end before the counts
  fn listed(rows: &[Row<'_>], verbose: bool, width: u16) -> Vec<String> {
    let took = Duration::from_secs(30);
    let text = tables(rows, false, verbose, took, None, false, width);
    let lines = text.lines().take_while(|line| !line.is_empty());
    lines.map(str::to_string).collect()
  }

  #[test]
  fn tables_list_the_flagged_and_the_unread_and_end_with_the_counts() {
    let (honest, fake) = honest_and_fake();
    let rows = [
      row("album/ev\u{1b}[31mil\n.flac", None, &fake),
      row("album/honest.flac", None, &honest),
      Row {
        path: "album/text.flac".into(),
        root: None,
        outcome: Err("the container was not recognized".to_string()),
      },
    ];
    let took = Duration::from_secs(30);
    let text = tables(&rows, false, false, took, None, false, 100);
    let lines: Vec<&str> = text.lines().collect();
    let cells = |line: &str| -> Vec<String> {
      let cells = line.split("  ").filter(|cell| !cell.is_empty());
      cells.map(|cell| cell.trim().to_string()).collect()
    };
    // the one flagged file under its directory, its name with nothing a terminal would act on
    assert_eq!(
      cells(lines[0]),
      ["Verdict", "Grid", "Cutoff", "Encoder", "File"]
    );
    assert!(lines[1].starts_with("  ----------------  ----  "), "{text}");
    assert_eq!(lines[2], "album/");
    let flagged = cells(lines[3]);
    assert_eq!(flagged[..2], ["likely transcode", "MP3"]);
    assert_eq!(flagged[3..], ["MP3 ~128 kbps", "ev\\u{1b}[31mil\\n.flac"]);
    assert!(!text.contains('\u{1b}'), "{text}");
    assert_eq!(
      lines[4..8],
      [
        "",
        "Not read",
        "--------",
        "album/text.flac: the container was not recognized"
      ]
    );
    // the counts come last
    assert_eq!(
      lines[8..],
      [
        "",
        "Verdict            Files",
        "------------------ -------",
        "consistent             1",
        "likely transcode       1",
        "mixed sources          0",
        "possibly lossy         0",
        "inconclusive           0",
        "",
        "Files read            2",
        "Files not read        1",
        "Wall time          30 s",
        "Files per minute      6",
      ]
    );

    // an interrupt is said with the totals
    let left = Some("5 of 8 files left");
    let cut_short = tables(&rows, false, false, took, left, false, 100);
    assert_eq!(
      cells(cut_short.lines().last().unwrap()),
      ["Interrupted", "5 of 8 files left"]
    );

    // colour changes no column
    let coloured = tables(&rows, false, false, took, None, true, 100);
    assert!(
      coloured.contains("\u{1b}[31mlikely transcode\u{1b}[0m"),
      "{coloured}"
    );
    assert_eq!(uncoloured(&coloured), text);

    // with nothing flagged the counts are all there is
    let clean = [
      row("a/1.flac", None, &honest),
      row("a/2.flac", None, &honest),
    ];
    let text = tables(&clean, false, false, took, None, false, 100);
    assert!(text.starts_with("Verdict            Files\n"), "{text}");
  }

  #[test]
  fn files_are_listed_by_directory_from_the_directory_named() {
    let (honest, fake) = honest_and_fake();
    let mut cut_lower = fake.clone();
    cut_lower.readings[0].reading.cutoff_hz = Some(15_500.0);
    let mut possibly = fake.clone();
    possibly.readings[0].reading.verdict = Verdict::PossiblyLossy;
    let rows = [
      // named itself, alone in its directory and beside another
      row("loose/one.flac", None, &fake),
      row("pair/a.flac", None, &fake),
      row("pair/b.flac", None, &honest),
      // found under a directory named: in it, and in directories under it
      row("music/top.flac", Some("music"), &fake),
      row("music/mixed/1.flac", Some("music"), &fake),
      row("music/mixed/2.flac", Some("music"), &honest),
      row("music/mixed/3.flac", Some("music"), &possibly),
      row("music/same/1.flac", Some("music"), &fake),
      row("music/same/2.flac", Some("music"), &cut_lower),
      row("music/clean/1.flac", Some("music"), &honest),
      row("other/deep/down/1.flac", Some("other/"), &fake),
    ];
    let lines = listed(&rows, false, 100);
    let names: Vec<&str> = lines[2..]
      .iter()
      .map(|line| match line.strip_prefix("  ") {
        // a file's line ends with its name
        Some(file) => file.rsplit("  ").next().unwrap(),
        None => line,
      })
      .collect();
    let cutoff = khz(fake.readings[0].reading.cutoff_hz.unwrap());
    let alike =
      format!("same/  2 of 2 likely transcode (MP3 grid, cutoff 15.5 to {cutoff}, MP3 ~128 kbps)");
    assert_eq!(
      names,
      [
        "loose/one.flac",
        "pair/",
        "a.flac",
        "music/",
        "top.flac",
        "mixed/",
        "1.flac",
        "3.flac",
        alike.as_str(),
        "other/",
        "deep/down/",
        "1.flac",
      ]
    );
    assert!(
      lines[2].starts_with("  likely transcode  MP3   "),
      "{lines:?}"
    );

    // every file and each grid's offset when verbose, and no directory on one line
    let lines = listed(&rows, true, 100);
    assert_eq!(lines.len(), 2 + 7 + rows.len(), "{lines:?}");
    assert!(lines.iter().any(|line| line.contains("  MP3, offset ")));
    assert!(lines.iter().all(|line| !line.contains(" of ")), "{lines:?}");
  }

  #[test]
  fn the_list_fits_the_terminal_and_gives_way_to_lines_in_a_narrow_one() {
    let (_, fake) = honest_and_fake();
    let long = "a-recording-with-a-long-name-that-no-terminal-of-80-columns-holds.24.flac";
    let rows = [
      row(&format!("music/{long}"), Some("music"), &fake),
      row("music/short.flac", Some("music"), &fake),
    ];
    let lines = listed(&rows, true, 80);
    assert!(lines.iter().all(|line| line.len() <= 80), "{lines:?}");
    let cut_short = lines.iter().find(|line| line.contains("...")).unwrap();
    assert_eq!(cut_short.len(), 80);
    assert!(cut_short.ends_with(".24.flac"), "{cut_short}");
    assert!(cut_short.contains("  a-recor"), "{cut_short}");

    // under 60 columns each file is the line a pipe gets, its path whole
    let lines = listed(&rows, false, 59);
    assert_eq!(lines.len(), 2);
    assert!(
      lines[0].starts_with(&format!(
        "music/{long}: likely transcode (MP3 grid, cutoff "
      )),
      "{lines:?}"
    );
  }

  #[test]
  fn a_cut_keeps_both_ends_within_the_width() {
    assert_eq!(cut("short.flac", 10), "short.flac");
    assert_eq!(cut("0123456789abcdef", 10), "0123...def");
    assert_eq!(cut("0123456789abcdef", 9), "012...def");
    // a wide character counts as its two columns and is never split
    assert_eq!(
      cut("\u{97f3}\u{697d}\u{97f3}\u{697d}\u{97f3}\u{697d}.flac", 12),
      "\u{97f3}\u{697d}...flac"
    );
    assert_eq!(cut("abcdef", 2), "...");
  }

  #[test]
  fn the_bars_take_at_most_half_the_terminal() {
    // 12 lines: the overall bar and 11 files
    assert_eq!(room(24, true, 11), 11);
    // one more file than fits gives a line to the count of the rest
    assert_eq!(room(24, true, 12), 10);
    assert_eq!(room(24, false, 12), 12);
    assert_eq!(room(24, false, 32), 11);
    // a terminal too small for any of it still shows one file
    assert_eq!(room(1, true, 5), 1);
    assert_eq!(room(0, false, 1), 1);
  }
}
