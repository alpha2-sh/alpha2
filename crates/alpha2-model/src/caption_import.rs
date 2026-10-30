//! SRT and WebVTT import: subtitle files to caption cues.
//!
//! Timestamps are milliseconds, and a millisecond is exactly 120 ticks, so
//! import is lossless. Markup that says who speaks or what a word is becomes
//! the cue's speaker and its text's runs: a WebVTT voice (`<v Name>`) names
//! the speaker and gives its text the class `voice` and the voice's classes,
//! `<c.a.b>` gives classes `a` and `b`, `<b>`, `<i>` and `<u>` (in SRT too)
//! give classes `b`, `i` and `u`, and WebVTT's inline timestamps split the
//! text into runs timed from the cue's start. The rest is dropped: SRT's other
//! tags and `{\...}` override blocks, WebVTT's `ruby`, `rt` and `lang` tags
//! (their text kept) and cue settings. A cue without such markup imports as
//! plain text. WebVTT's character references are decoded. A `<` in SRT text
//! that opens no tag is text. A block that does not parse is skipped and
//! counted; the rest of the file still imports.
//!
//! A line ends at a line feed, a carriage return, or both, as WebVTT
//! defines; carriage returns repeated before a line feed (a file whose line
//! endings were converted twice) end one line.
//!
//! A file is read in time linear in its size, and a file larger than
//! [`MAX_CAPTION_FILE_BYTES`] or with more than [`MAX_CAPTION_FILE_CUES`]
//! cues is refused before it is read further.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::{
    CaptionCue, MAX_CLIPS_PER_TRACK, MAX_PROJECT_JSON_BYTES, MAX_RUN_CLASSES,
    MAX_TEXT_CONTENT_BYTES, MAX_TEXT_RUNS, MAX_TRACKS, RunTime, TextContent, TextRun,
    valid_class_name,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND, TimeRange};

/// The largest subtitle file read, in bytes: a project document's limit.
/// A cue's JSON is longer than its subtitle block, so a larger file holds
/// more cues than a project can.
pub const MAX_CAPTION_FILE_BYTES: usize = MAX_PROJECT_JSON_BYTES;

/// The most cues read from one subtitle file: a day of continuous
/// captions at a cue every 1.3 s, and far more than a feature film's few
/// thousand.
pub const MAX_CAPTION_FILE_CUES: usize = 65_536;

/// Cues read from a subtitle file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportedCaptions {
    /// Cues in file order, sorted by start time. They may overlap; see
    /// [`caption_rows`].
    pub cues: Vec<CaptionCue>,
    /// Blocks that were not cues and could not be read as one.
    pub skipped: usize,
    /// Markup read but not kept.
    pub notes: ImportNotes,
}

/// What an import read but could not keep, counted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportNotes {
    /// Voices after a cue's first, whose names are not its speaker: their
    /// text still takes the class `voice`.
    pub extra_voices: usize,
    /// Inline timestamps out of order or outside their cue, which WebVTT
    /// renderers ignore.
    pub dropped_timestamps: usize,
    /// Class names a run cannot carry (not `A-Z a-z 0-9 _ -`, longer than 64
    /// bytes, or past a run's [`MAX_RUN_CLASSES`]).
    pub dropped_classes: usize,
    /// Cues with more runs than a text holds ([`MAX_TEXT_RUNS`]), imported
    /// as plain text.
    pub flattened: usize,
}

/// Why a file is not a subtitle file at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CaptionImportError {
    /// A WebVTT file does not begin with `WEBVTT`.
    MissingWebVttHeader,
    /// The file is larger than [`MAX_CAPTION_FILE_BYTES`].
    FileTooLarge {
        /// The file's size in bytes.
        bytes: usize,
        /// The largest file read.
        maximum: usize,
    },
    /// The file holds more than [`MAX_CAPTION_FILE_CUES`] cues.
    TooManyCues {
        /// The most cues read from a file.
        maximum: usize,
    },
    /// The cues need more caption tracks than a project holds: too many
    /// overlap at once, or there are too many to fit
    /// [`MAX_CLIPS_PER_TRACK`] to a track.
    TooManyRows {
        /// Caption tracks the cues need.
        rows: usize,
        /// Tracks a project holds.
        maximum: usize,
    },
}

impl std::fmt::Display for CaptionImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingWebVttHeader => formatter.write_str("a WebVTT file begins with WEBVTT"),
            Self::FileTooLarge { bytes, maximum } => write!(
                formatter,
                "the subtitle file is {bytes} bytes; at most {maximum} are read"
            ),
            Self::TooManyCues { maximum } => write!(
                formatter,
                "the subtitle file holds more than {maximum} cues"
            ),
            Self::TooManyRows { rows, maximum } => write!(
                formatter,
                "the captions need {rows} caption tracks; a project holds at most {maximum} tracks"
            ),
        }
    }
}

impl std::error::Error for CaptionImportError {}

const TICKS_PER_MILLISECOND: i64 = TICKS_PER_SECOND / 1_000;

/// Reads SubRip (`.srt`) text.
///
/// # Errors
/// Returns [`CaptionImportError::FileTooLarge`] or
/// [`CaptionImportError::TooManyCues`] for a file past the import limits.
pub fn parse_srt(text: &str) -> Result<ImportedCaptions, CaptionImportError> {
    check_file_size(text)?;
    let mut imported = ImportedCaptions::default();
    for block in blocks(text) {
        let mut lines = block.iter().copied();
        let Some(mut first) = lines.next() else {
            continue;
        };
        // An optional sequence number before the timing line.
        if !first.contains("-->") {
            match lines.next() {
                Some(line) if first.trim().chars().all(|c| c.is_ascii_digit()) => first = line,
                _ => {
                    imported.skipped += 1;
                    continue;
                }
            }
        }
        let body = lines.collect::<Vec<_>>();
        let text = body
            .iter()
            .map(|line| strip_srt_markup(line))
            .collect::<Vec<_>>()
            .join("\n");
        let range = timing(first);
        let marked = range.map(|_| srt_markup(&body));
        imported.read(range, &text, marked)?;
    }
    imported.cues.sort_by_key(|cue| cue.timeline_range.start());
    Ok(imported)
}

/// Reads WebVTT (`.vtt`) text. `NOTE`, `STYLE`, and `REGION` blocks are
/// ignored.
///
/// # Errors
/// Returns [`CaptionImportError::MissingWebVttHeader`] unless the text
/// begins with `WEBVTT`, and [`CaptionImportError::FileTooLarge`] or
/// [`CaptionImportError::TooManyCues`] for a file past the import limits.
pub fn parse_webvtt(text: &str) -> Result<ImportedCaptions, CaptionImportError> {
    check_file_size(text)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let header_ok = text
        .strip_prefix("WEBVTT")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r']));
    if !header_ok {
        return Err(CaptionImportError::MissingWebVttHeader);
    }
    let mut imported = ImportedCaptions::default();
    let mut blocks = blocks(text).into_iter();
    // The header is the first block, up to a timing line: as in the WebVTT parser, a cue may
    // follow it without a blank line.
    let header = blocks.next().unwrap_or_default();
    let cue_under_header = header
        .iter()
        .position(|line| line.contains("-->"))
        .map(|start| header[start..].to_vec());
    for block in cue_under_header.into_iter().chain(blocks) {
        let first = block[0];
        if ["NOTE", "STYLE", "REGION"].iter().any(|keyword| {
            first
                .strip_prefix(keyword)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        }) {
            continue;
        }
        // An optional cue identifier before the timing line.
        let (timing_line, body) = if first.contains("-->") {
            (first, &block[1..])
        } else if block.len() > 1 && block[1].contains("-->") {
            (block[1], &block[2..])
        } else {
            imported.skipped += 1;
            continue;
        };
        let text = body
            .iter()
            .map(|line| decode_references(&strip_tags(line)))
            .collect::<Vec<_>>()
            .join("\n");
        let range = timing(timing_line);
        let marked = range.map(|range| vtt_markup(&body.join("\n"), range));
        imported.read(range, &text, marked)?;
    }
    imported.cues.sort_by_key(|cue| cue.timeline_range.start());
    Ok(imported)
}

impl ImportedCaptions {
    /// Keeps a block's cue, or counts it skipped when its timing or text is
    /// not a cue's. `text` is the cue's text without markup; `marked`, what
    /// its markup says, gives the runs and speaker when it says anything.
    fn read(
        &mut self,
        range: Option<TimeRange>,
        text: &str,
        marked: Option<Marked>,
    ) -> Result<(), CaptionImportError> {
        match (range, cue_text(text)) {
            (Some(range), Some(text)) => {
                if self.cues.len() == MAX_CAPTION_FILE_CUES {
                    return Err(CaptionImportError::TooManyCues {
                        maximum: MAX_CAPTION_FILE_CUES,
                    });
                }
                let mut cue = CaptionCue::new(range, text);
                if let Some(marked) = marked {
                    let notes = &mut self.notes;
                    notes.extra_voices += marked.notes.extra_voices;
                    notes.dropped_timestamps += marked.notes.dropped_timestamps;
                    notes.dropped_classes += marked.notes.dropped_classes;
                    cue.speaker = marked.speaker;
                    match marked.runs {
                        Some(runs) if runs.len() > MAX_TEXT_RUNS => notes.flattened += 1,
                        Some(runs) => cue.text = TextContent::Runs(runs),
                        None => {}
                    }
                }
                self.cues.push(cue);
            }
            _ => self.skipped += 1,
        }
        Ok(())
    }
}

fn check_file_size(text: &str) -> Result<(), CaptionImportError> {
    if text.len() > MAX_CAPTION_FILE_BYTES {
        return Err(CaptionImportError::FileTooLarge {
            bytes: text.len(),
            maximum: MAX_CAPTION_FILE_BYTES,
        });
    }
    Ok(())
}

/// Splits cues onto rows, each row a caption track's worth: every cue goes
/// to the first row that has room for another cue (a track holds
/// [`MAX_CLIPS_PER_TRACK`]) and whose last cue has ended by its start.
///
/// Takes time `n log n` in the cues, however many overlap.
///
/// # Errors
/// Returns [`CaptionImportError::TooManyRows`] for cues that need more rows
/// than a project holds tracks.
pub fn caption_rows(cues: &[CaptionCue]) -> Result<Vec<Vec<CaptionCue>>, CaptionImportError> {
    let mut sorted = cues.to_vec();
    sorted.sort_by_key(|cue| cue.timeline_range.start());
    let mut rows: Vec<Vec<CaptionCue>> = Vec::new();
    // Rows with room, apart: those whose last cue has ended by the cue being placed, and the
    // others by when theirs ends. A full row is in neither.
    let mut ended = BTreeSet::<usize>::new();
    let mut ending = BinaryHeap::<Reverse<(MediaTime, usize)>>::new();
    for cue in sorted {
        let start = cue.timeline_range.start();
        while let Some(&Reverse((end, row))) = ending.peek()
            && end <= start
        {
            ending.pop();
            ended.insert(row);
        }
        let row = ended.pop_first().unwrap_or_else(|| {
            rows.push(Vec::new());
            rows.len() - 1
        });
        let end = cue.timeline_range.end();
        rows[row].push(cue);
        if rows[row].len() < MAX_CLIPS_PER_TRACK {
            ending.push(Reverse((end, row)));
        }
    }
    if rows.len() > MAX_TRACKS {
        return Err(CaptionImportError::TooManyRows {
            rows: rows.len(),
            maximum: MAX_TRACKS,
        });
    }
    Ok(rows)
}

/// Blank-line-separated blocks of lines.
fn blocks(text: &str) -> Vec<Vec<&str>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for line in lines(text) {
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// The lines of `text`. A line feed, a carriage return, or a carriage return
/// and a line feed ends a line; so does a run of carriage returns followed by
/// a line feed. A run that is not followed by one ends a line per return.
fn lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let (mut start, mut at) = (0, 0);
    while at < bytes.len() {
        match bytes[at] {
            b'\n' => {
                lines.push(&text[start..at]);
                at += 1;
                start = at;
            }
            b'\r' => {
                let returns = bytes[at..]
                    .iter()
                    .take_while(|&&byte| byte == b'\r')
                    .count();
                lines.push(&text[start..at]);
                at += returns;
                if bytes.get(at) == Some(&b'\n') {
                    at += 1;
                } else {
                    lines.extend(std::iter::repeat_n("", returns - 1));
                }
                start = at;
            }
            _ => at += 1,
        }
    }
    lines.push(&text[start..]);
    lines
}

/// `start --> end`, ignoring anything after the end time (WebVTT settings).
fn timing(line: &str) -> Option<TimeRange> {
    let (start, rest) = line.split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    let (start, end) = (timestamp(start.trim())?, timestamp(end)?);
    TimeRange::new(start, end)
        .ok()
        .filter(|range| !range.is_empty())
}

/// `[hh:]mm:ss[,.]mmm`, hours any number of digits, up to the latest time a
/// project document holds ([`alpha2_time::MAX_SERIALIZED_TICKS`]): a cue
/// past it could not be saved, so its block does not parse.
fn timestamp(text: &str) -> Option<MediaTime> {
    let (clock, millis) = text.split_once([',', '.'])?;
    if millis.len() != 3 || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let fields = clock.split(':').collect::<Vec<_>>();
    let number = |field: &str| {
        (!field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| field.parse::<i64>().ok())
            .flatten()
    };
    let (hours, minutes, seconds) = match fields.as_slice() {
        [hours, minutes, seconds] => (number(hours)?, number(minutes)?, number(seconds)?),
        [minutes, seconds] => (0, number(minutes)?, number(seconds)?),
        _ => return None,
    };
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let milliseconds = ((hours.checked_mul(60)?.checked_add(minutes)?)
        .checked_mul(60)?
        .checked_add(seconds)?)
    .checked_mul(1_000)?
    .checked_add(millis.parse::<i64>().ok()?)?;
    let ticks = milliseconds.checked_mul(TICKS_PER_MILLISECOND)?;
    (ticks <= alpha2_time::MAX_SERIALIZED_TICKS).then(|| MediaTime::from_ticks(ticks))
}

/// Cue text with blank lines around it trimmed, or `None` when nothing is
/// left or it is too long.
fn cue_text(text: &str) -> Option<String> {
    let text = text.trim_matches('\n').to_owned();
    (!text.trim().is_empty() && text.len() <= MAX_TEXT_CONTENT_BYTES).then_some(text)
}

/// SRT text without `<...>` tags or `{\...}` override blocks.
fn strip_srt_markup(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("{\\") {
        output.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
            }
        }
    }
    output.push_str(rest);
    strip_srt_tags(&output)
}

/// SRT text without its tags: a `<`, an optional `/`, a letter, and then
/// anything but `<` up to a `>`. Any other `<` is text, since SubRip has no
/// escape for it.
fn strip_srt_tags(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('<') {
        output.push_str(&rest[..start]);
        let tag = &rest[start + 1..];
        let name = tag.strip_prefix('/').unwrap_or(tag);
        // Each `<` looks ahead only to the next `<` or `>`, so a line is read in time linear in
        // its length.
        let end = tag.find(['<', '>']);
        match end {
            Some(end)
                if tag.as_bytes()[end] == b'>'
                    && name.starts_with(|character: char| character.is_ascii_alphabetic()) =>
            {
                rest = &tag[end + 1..];
            }
            _ => {
                output.push('<');
                rest = tag;
            }
        }
    }
    output.push_str(rest);
    output
}

/// WebVTT text without `<...>` tags: in WebVTT a `<` always opens one.
fn strip_tags(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut depth = false;
    for character in line.chars() {
        match character {
            '<' => depth = true,
            '>' if depth => depth = false,
            _ if !depth => output.push(character),
            _ => {}
        }
    }
    output
}

/// WebVTT's character references: the named ones it defines and numeric
/// ones. An unknown reference stays as written.
///
/// A reference this reads is letters, digits, `#`, and `+` closed by `;`, so
/// each `&` looks only that far: a `;` past them closes nothing, and the
/// text is read in time linear in its length.
fn decode_references(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let length = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'#' | b'+'))
            .count();
        let decoded = (rest.as_bytes().get(length) == Some(&b';'))
            .then(|| reference(&rest[..length]))
            .flatten();
        match decoded {
            Some(character) => {
                output.push(character);
                rest = &rest[length + 1..];
            }
            None => output.push('&'),
        }
    }
    output.push_str(rest);
    output
}

/// The character a reference's name, between `&` and `;`, stands for.
fn reference(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "nbsp" => Some('\u{a0}'),
        "lrm" => Some('\u{200e}'),
        "rlm" => Some('\u{200f}'),
        _ => name
            .strip_prefix("#x")
            .or_else(|| name.strip_prefix("#X"))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| name.strip_prefix('#').and_then(|dec| dec.parse().ok()))
            .and_then(char::from_u32),
    }
}

/// What a cue's markup says: its speaker, its runs when any carries a
/// class or a time (else the plain text stands), and what was dropped.
struct Marked {
    speaker: Option<String>,
    runs: Option<Vec<TextRun>>,
    notes: ImportNotes,
}

/// Builds a cue's runs from text pieces, each with the classes open over it
/// and its time segment.
#[derive(Default)]
struct Runs {
    pieces: Vec<(String, Vec<String>, usize)>,
    notes: ImportNotes,
}

impl Runs {
    fn push(&mut self, text: &str, classes: &[String], segment: usize) {
        if text.is_empty() {
            return;
        }
        match self.pieces.last_mut() {
            Some((held, open, at)) if open.as_slice() == classes && *at == segment => {
                held.push_str(text);
            }
            _ => self
                .pieces
                .push((text.to_owned(), classes.to_vec(), segment)),
        }
    }

    /// The runs, timed by `bounds` (each segment's start, then the end, in
    /// ticks from the cue's start) when there are segments, with blank lines
    /// around the text trimmed as plain cue text is; `None` when no run
    /// carries a class or a time.
    fn finish(mut self, bounds: &[i64]) -> (Option<Vec<TextRun>>, ImportNotes) {
        // Trim leading and trailing line feeds across pieces, as `cue_text` trims the plain text.
        while let Some(first) = self.pieces.first_mut() {
            let trimmed = first.0.trim_start_matches('\n').len();
            first.0.drain(..first.0.len() - trimmed);
            if first.0.is_empty() {
                self.pieces.remove(0);
            } else {
                break;
            }
        }
        while let Some(last) = self.pieces.last_mut() {
            let kept = last.0.trim_end_matches('\n').len();
            last.0.truncate(kept);
            if last.0.is_empty() {
                self.pieces.pop();
            } else {
                break;
            }
        }
        let timed = bounds.len() > 2;
        if !timed && self.pieces.iter().all(|(_, classes, _)| classes.is_empty()) {
            return (None, self.notes);
        }
        let runs = self
            .pieces
            .into_iter()
            .map(|(text, mut class, segment)| {
                if class.len() > MAX_RUN_CLASSES {
                    self.notes.dropped_classes += class.len() - MAX_RUN_CLASSES;
                    class.truncate(MAX_RUN_CLASSES);
                }
                TextRun {
                    text,
                    class,
                    time: timed.then(|| RunTime {
                        start: MediaTime(bounds[segment]),
                        end: MediaTime(bounds[segment + 1]),
                    }),
                }
            })
            .collect();
        (Some(runs), self.notes)
    }
}

/// The classes a tag's dot-separated `names` give, the ones a run cannot
/// carry counted and dropped.
fn tag_classes(names: &str, notes: &mut ImportNotes) -> Vec<String> {
    names
        .split('.')
        .filter(|name| !name.is_empty())
        .filter_map(|name| {
            if valid_class_name(name) {
                Some(name.to_owned())
            } else {
                notes.dropped_classes += 1;
                None
            }
        })
        .collect()
}

/// A WebVTT cue body's markup, the cue over `range`: voices, classes, bold,
/// italic and underline as run classes; inline timestamps splitting the runs.
fn vtt_markup(body: &str, range: TimeRange) -> Marked {
    let start = range.start().ticks();
    let duration = range.duration().map_or(0, MediaTime::ticks);
    let mut runs = Runs::default();
    let mut speaker = None;
    // The tags open, each with the classes it gives, innermost last.
    let mut open: Vec<(String, Vec<String>)> = Vec::new();
    let mut bounds = vec![0_i64];
    let mut rest = body;
    let classes = |open: &[(String, Vec<String>)]| {
        open.iter()
            .flat_map(|(_, classes)| classes.iter().cloned())
            .fold(Vec::<String>::new(), |mut all, class| {
                if !all.contains(&class) {
                    all.push(class);
                }
                all
            })
    };
    loop {
        let Some(at) = rest.find('<') else {
            runs.push(&decode_references(rest), &classes(&open), bounds.len() - 1);
            break;
        };
        runs.push(
            &decode_references(&rest[..at]),
            &classes(&open),
            bounds.len() - 1,
        );
        let Some(end) = rest[at..].find('>') else {
            break;
        };
        let tag = &rest[at + 1..at + end];
        rest = &rest[at + end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            if let Some(index) = open.iter().rposition(|(open, _)| open == name) {
                open.truncate(index);
            }
            continue;
        }
        if tag.starts_with(|character: char| character.is_ascii_digit()) {
            // An inline timestamp: kept only in order and inside the cue.
            match timestamp(tag.trim()) {
                Some(time)
                    if time.ticks() - start > *bounds.last().unwrap_or(&0)
                        && time.ticks() - start < duration =>
                {
                    bounds.push(time.ticks() - start);
                }
                _ => runs.notes.dropped_timestamps += 1,
            }
            continue;
        }
        // `name.classes annotation`.
        let (head, annotation) = tag
            .split_once([' ', '\t'])
            .map_or((tag, ""), |(head, annotation)| (head, annotation.trim()));
        let (name, names) = head.split_once('.').unwrap_or((head, ""));
        let given = match name {
            "v" => {
                if speaker.is_none() && !annotation.is_empty() {
                    speaker = Some(annotation.to_owned());
                } else if !annotation.is_empty() {
                    runs.notes.extra_voices += 1;
                }
                let mut given = vec!["voice".to_owned()];
                given.extend(tag_classes(names, &mut runs.notes));
                given
            }
            "c" => tag_classes(names, &mut runs.notes),
            "b" | "i" | "u" => {
                let mut given = vec![name.to_owned()];
                given.extend(tag_classes(names, &mut runs.notes));
                given
            }
            _ => Vec::new(),
        };
        open.push((name.to_owned(), given));
    }
    bounds.push(duration);
    let (runs, notes) = runs.finish(&bounds);
    Marked {
        speaker: speaker.filter(|speaker| speaker.len() <= crate::MAX_CAPTION_NAME_BYTES),
        runs,
        notes,
    }
}

/// An SRT cue body's markup: `<b>`, `<i>` and `<u>` as run classes; other
/// tags and override blocks dropped.
fn srt_markup(body: &[&str]) -> Marked {
    let mut runs = Runs::default();
    let mut open: Vec<String> = Vec::new();
    for (index, line) in body.iter().enumerate() {
        if index > 0 {
            runs.push("\n", &open, 0);
        }
        let line = strip_overrides(line);
        let mut rest = line.as_str();
        while let Some(start) = rest.find('<') {
            runs.push(&rest[..start], &open, 0);
            let tag = &rest[start + 1..];
            let closing = tag.starts_with('/');
            let name = tag.strip_prefix('/').unwrap_or(tag);
            match tag.find(['<', '>']) {
                Some(end)
                    if tag.as_bytes()[end] == b'>'
                        && name.starts_with(|character: char| character.is_ascii_alphabetic()) =>
                {
                    let tag_name = name[..name.len().min(end - usize::from(closing))]
                        .split([' ', '\t'])
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if matches!(tag_name.as_str(), "b" | "i" | "u") {
                        if closing {
                            if let Some(index) = open.iter().rposition(|open| *open == tag_name) {
                                open.remove(index);
                            }
                        } else if !open.contains(&tag_name) {
                            open.push(tag_name);
                        }
                    }
                    rest = &tag[end + 1..];
                }
                _ => {
                    runs.push("<", &open, 0);
                    rest = tag;
                }
            }
        }
        runs.push(rest, &open, 0);
    }
    let (runs, notes) = runs.finish(&[0, 0]);
    Marked {
        speaker: None,
        runs,
        notes,
    }
}

/// SRT text without its `{\...}` override blocks.
fn strip_overrides(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("{\\") {
        output.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
            }
        }
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
#[path = "caption_import_tests.rs"]
mod tests;
