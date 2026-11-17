//! what a check shows in a terminal: a bar per file under way over one for them all, then one
//! file's reading in detail or the tables of many

use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anstyle::{AnsiColor, Style};
use choon::grid::Evidence;
use choon::{OpusUse, Reading, Verdict};
use comfy_table::{CellAlignment, ContentArrangement, LineStyle, Table, TableStyle};
use console::{Alignment, Term};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::{Checked, flag_words, flagged, label, named_guess};

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
  pub path: &'a Path,
  pub outcome: Result<Box<Checked>, String>,
}

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
/// under them one bar for all the files
pub struct Bars {
  multi: MultiProgress,
  term: Term,
  /// `None` for a single file
  overall: Option<ProgressBar>,
  started: Instant,
  /// the line counting the files under way that have no bar
  more: Option<ProgressBar>,
  /// in the order they started
  flying: Vec<Flying>,
}

/// how wide a file's bar is drawn, in columns
const BAR: usize = 20;
/// the columns a file's line holds beside its name: a space, the bar in brackets, a space and
/// the percentage
const BESIDE_NAME: usize = BAR + 8;

fn style(template: &str) -> ProgressStyle {
  ProgressStyle::with_template(template)
    .expect("the template is well formed")
    .progress_chars("=> ")
}

impl Bars {
  pub fn new(files: usize) -> Self {
    let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stdout());
    let overall = (files > 1).then(|| {
      let style = style("[{wide_bar}] {pos}/{len} files{msg}");
      let overall = multi.add(ProgressBar::new(files as u64).with_style(style));
      // a bar is drawn from its first change on, which without this is the first file's end
      overall.tick();
      overall
    });
    Self {
      multi,
      term: Term::stdout(),
      overall,
      started: Instant::now(),
      more: None,
      flying: Vec::new(),
    }
  }

  pub fn started(&mut self, file: usize, name: String, progress: Vec<Arc<AtomicU32>>) {
    self.flying.push(Flying {
      file,
      name,
      progress,
      bar: None,
    });
  }

  pub fn ended(&mut self, file: usize) {
    if let Some(at) = self.flying.iter().position(|flying| flying.file == file)
      && let Some(bar) = self.flying.remove(at).bar
    {
      remove(&self.multi, &bar);
    }
    if let Some(overall) = &self.overall {
      overall.inc(1);
      // the rate over the whole run so far: files end in bursts, which a rate over the last
      // few would follow
      let (done, all) = (overall.position(), overall.length().unwrap_or(0));
      let per_second = done as f64 / self.started.elapsed().as_secs_f64();
      let left = Duration::from_secs_f64((all - done) as f64 / per_second);
      overall.set_message(format!(
        ", {:.0} per minute, ETA {}",
        per_second * 60.0,
        length(left)
      ));
    }
  }

  /// brings the bars up to date with the files under way and the terminal's size
  pub fn tick(&mut self) {
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
  }

  /// prints `line` on stdout above the bars
  pub fn say(&self, line: &str) -> io::Result<()> {
    self
      .multi
      .suspend(|| writeln!(io::stdout().lock(), "{line}"))
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
/// counting the files without one and the overall bar take at most half the lines, which leaves
/// the rest to the findings above them
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

fn khz(hz: f64) -> String {
  format!("{:.1} kHz", hz / 1000.0)
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
/// and its value, wrapped at `width` columns
pub fn readout(path: &Path, checked: &Checked, profiled: bool, colour: bool, width: u16) -> String {
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
    if let Some(guess) = named_guess(reading) {
      add("Encoder guess", guess.to_string());
    }
    if reading.verdict != Verdict::Inconclusive {
      add("Confidence", format!("{:.2}", reading.confidence));
    }
    if !reading.flags.is_empty() {
      let flags: Vec<&str> = reading.flags.iter().copied().map(flag_words).collect();
      add("Flags", flags.join(", "));
    }
    add("Summary", reading.summary.clone());
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
      Evidence::Count(scan) => format!(
        "{} at offset {}, z {:.0}, {} of {} stretches agree",
        found.codec,
        scan.offset,
        scan.z,
        scan.segments_agreeing,
        scan.segment_z.len()
      ),
      Evidence::Lattice(scan) => format!(
        "{} at offset {}, {} frames on its energy lattice in the stretches, {} in the windows",
        found.codec, scan.offset, scan.frames, scan.uncut_frames
      ),
    },
    (None, Some(scan)) => {
      let opus = scan.opus.as_ref().map_or_else(String::new, |opus| {
        format!(", Opus frames {} and {}", opus.frames, opus.uncut_frames)
      });
      format!(
        "none over the thresholds (MP3 z {:.1}, AAC z {:.1}{opus})",
        scan.mp3.z, scan.aac.z
      )
    }
    (None, None) => "not scanned".to_string(),
  }
}

/// what a check of many files found: the count of each verdict, under each profile when
/// `profiled`, the files read and not read and how long it took, then the flagged files and
/// the files not read, and each flagged file in detail when `verbose`
pub fn tables(
  rows: &[Row<'_>],
  profiled: bool,
  verbose: bool,
  took: Duration,
  colour: bool,
  width: u16,
) -> String {
  let read: Vec<(&Path, &Checked)> = rows
    .iter()
    .filter_map(|row| Some((row.path, &**row.outcome.as_ref().ok()?)))
    .collect();
  let not_read: Vec<(&Path, &str)> = rows
    .iter()
    .filter_map(|row| Some((row.path, row.outcome.as_ref().err()?.as_str())))
    .collect();
  let mut text = String::from("\n");

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
    text += &lines(&mut counts, 0);
    text += "\n";
  }

  let mut totals = table(width);
  totals.add_row(["Files read", &read.len().to_string()]);
  totals.add_row(["Files not read", &not_read.len().to_string()]);
  totals.add_row(["Wall time", &length(took)]);
  totals.add_row([
    "Files per minute",
    &format!("{:.0}", rows.len() as f64 / took.as_secs_f64() * 60.0),
  ]);
  right_align(&mut totals, 1);
  text += &lines(&mut totals, 0);

  let found: Vec<(&Path, &Checked)> = read
    .iter()
    .copied()
    .filter(|(_, checked)| checked.readings.iter().any(|r| flagged(r.reading.verdict)))
    .collect();
  // a path comes last in its line and is never wrapped, so it can be copied whole
  if !found.is_empty() {
    let mut list = table(width);
    list.set_content_arrangement(ContentArrangement::Disabled);
    let mut header = vec!["Verdict", "Grid", "Cutoff", "Encoder guess"];
    if profiled {
      header.push("Profile");
    }
    header.push("Path");
    list.set_header(header);
    for (path, checked) in &found {
      for read in &checked.readings {
        let reading = &read.reading;
        if !flagged(reading.verdict) {
          continue;
        }
        let mut row = vec![
          painted(Some(reading.verdict), label(reading.verdict), colour),
          reading.grid.as_ref().map_or_else(String::new, |found| {
            format!("{}, offset {}", found.codec, found.evidence.offset())
          }),
          reading.cutoff_hz.map_or_else(String::new, khz),
          named_guess(reading).unwrap_or_default().to_string(),
        ];
        if profiled {
          row.push(read.profile.clone());
        }
        row.push(legible(path));
        list.add_row(row);
      }
    }
    text += "\n";
    text += &lines(&mut list, 0);
  }

  if !not_read.is_empty() {
    text += "\nNot read\n--------\n";
    for (path, why) in &not_read {
      text += &format!("{}: {why}\n", legible(path));
    }
  }

  if verbose && !found.is_empty() {
    text += "\n";
    for (path, checked) in &found {
      text += &readout(path, checked, profiled, colour, width);
    }
  }
  text
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
  use choon::{CancelCheck, Coverage};

  use super::{Row, legible, readout, room, tables};
  use crate::{Checked, Thresholds, analyzed};

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
      Box::new(analyzed(path, &profiles, Coverage::Full, &[Arc::default()], &never).unwrap())
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
  fn a_control_character_in_a_name_is_written_as_its_escape() {
    assert_eq!(
      legible(Path::new("a\nb\u{1b}[31m \u{e9}.flac")),
      "a\\nb\\u{1b}[31m \u{e9}.flac"
    );
  }

  #[test]
  fn a_readout_lines_up_its_values_and_wraps_them_under_themselves() {
    let (_, fake) = honest_and_fake();
    let text = readout(Path::new("album/fake.flac"), &fake, false, false, 60);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "album/fake.flac");
    assert!(
      lines.contains(&"  Verdict         likely transcode"),
      "{text}"
    );
    assert!(lines.contains(&"  Encoder guess   mp3 ~128 kbps"), "{text}");
    assert!(
      lines
        .iter()
        .any(|line| line.starts_with("  Grid            MP3 at offset ")),
      "{text}"
    );
    // the summary is longer than a line: what follows its first line starts under its value
    let summary = lines
      .iter()
      .position(|line| line.starts_with("  Summary"))
      .unwrap();
    assert!(
      lines[summary + 1].starts_with("                  "),
      "{text}"
    );
    assert!(lines.iter().all(|line| line.len() <= 60), "{text}");

    let coloured = readout(Path::new("album/fake.flac"), &fake, false, true, 60);
    assert!(
      coloured.contains("\u{1b}[31mlikely transcode\u{1b}[0m"),
      "{coloured}"
    );
    assert_eq!(uncoloured(&coloured), text);
  }

  #[test]
  fn tables_count_each_verdict_and_list_the_flagged_and_the_unread() {
    let (honest, fake) = honest_and_fake();
    let rows = [
      Row {
        path: Path::new("album/ev\u{1b}[31mil\n.flac"),
        outcome: Ok(fake),
      },
      Row {
        path: Path::new("album/honest.flac"),
        outcome: Ok(honest),
      },
      Row {
        path: Path::new("album/text.flac"),
        outcome: Err("the container was not recognized".to_string()),
      },
    ];
    let text = tables(&rows, false, false, Duration::from_secs(30), false, 100);
    let lines: Vec<&str> = text.lines().collect();
    let cells = |start: &str| -> Vec<&str> {
      let line = lines.iter().find(|line| line.starts_with(start));
      line
        .unwrap_or_else(|| panic!("{start}: {text}"))
        .split("  ")
        .filter(|cell| !cell.is_empty())
        .map(str::trim)
        .collect()
    };
    assert!(lines.contains(&"Verdict            Files"), "{text}");
    assert!(lines.contains(&"consistent             1"), "{text}");
    assert!(lines.contains(&"likely transcode       1"), "{text}");
    assert!(lines.contains(&"possibly lossy         0"), "{text}");
    assert_eq!(cells("Files read"), ["Files read", "2"]);
    assert_eq!(cells("Files not read"), ["Files not read", "1"]);
    assert_eq!(cells("Wall time"), ["Wall time", "30 s"]);
    assert_eq!(cells("Files per minute"), ["Files per minute", "6"]);
    assert_eq!(
      cells("Verdict            Grid"),
      ["Verdict", "Grid", "Cutoff", "Encoder guess", "Path"]
    );
    // the one flagged file, its name on one line with nothing a terminal would act on
    let flagged = cells("likely transcode   MP3");
    assert_eq!(
      flagged[3..],
      ["mp3 ~128 kbps", "album/ev\\u{1b}[31mil\\n.flac"]
    );
    assert!(!text.contains('\u{1b}'), "{text}");
    let unread = lines.iter().position(|line| *line == "Not read").unwrap();
    assert_eq!(
      lines[unread + 1..],
      [
        "--------",
        "album/text.flac: the container was not recognized"
      ]
    );

    // colour changes no column
    let coloured = tables(&rows, false, false, Duration::from_secs(30), true, 100);
    assert!(
      coloured.contains("\u{1b}[31mlikely transcode\u{1b}[0m"),
      "{coloured}"
    );
    assert_eq!(uncoloured(&coloured), text);
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
