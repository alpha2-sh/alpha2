//! SRT and WebVTT import: subtitle files to caption cues.
//!
//! A millisecond is exactly 120 ticks, so import is lossless. Markup that says who speaks or
//! what a word is becomes the cue's speaker and runs: a WebVTT voice (`<v Name>`) sets the
//! speaker and gives classes `voice` plus the voice's classes, `<c.a.b>` gives `a` and `b`,
//! `<b>`, `<i>` and `<u>` (also in SRT) give `b`, `i` and `u`, and WebVTT inline timestamps
//! split the text into runs timed from the cue's start. Other markup is dropped: SRT's other
//! tags and `{\...}` overrides, WebVTT's `ruby`, `rt` and `lang` tags (text kept) and cue
//! settings. WebVTT character references are decoded. A `<` in SRT that opens no tag is
//! text. Blocks that do not parse are skipped and counted.
//!
//! A line ends at LF, CR or CRLF, as WebVTT defines; repeated CRs before an LF (line endings
//! converted twice) end one line.
//!
//! Reading is linear in file size. Files over [`MAX_CAPTION_FILE_BYTES`] or with more than
//! [`MAX_CAPTION_FILE_CUES`] cues are refused early. A block over [`MAX_CUE_BLOCK_BYTES`] or
//! [`MAX_CUE_BLOCK_LINES`] is skipped without being held. One pass over a cue's markup gives both
//! its text and its runs; a cue with more than [`MAX_OPEN_TAGS`] tags open, [`MAX_HELD_CLASSES`]
//! classes held or [`MAX_TEXT_RUNS`] runs is imported as plain text.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::{
    CaptionCue, MAX_CLIPS_PER_TRACK, MAX_PROJECT_JSON_BYTES, MAX_RUN_CLASSES,
    MAX_TEXT_CONTENT_BYTES, MAX_TEXT_RUNS, MAX_TRACKS, RunTime, TextContent, TextRun,
    valid_class_name,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND, TimeRange};

/// The largest subtitle file read, in bytes. A cue's JSON is longer than its subtitle block,
/// so a larger file holds more cues than a project can.
pub const MAX_CAPTION_FILE_BYTES: usize = MAX_PROJECT_JSON_BYTES;

/// The most cues read from one subtitle file: a day of continuous captions at one cue every
/// 1.3 s.
pub const MAX_CAPTION_FILE_CUES: usize = 65_536;

/// The longest block read as a cue, in bytes; a longer one is skipped.
pub const MAX_CUE_BLOCK_BYTES: usize = 65_536;

/// The most lines of a block read as a cue; a longer one is skipped.
pub const MAX_CUE_BLOCK_LINES: usize = 1_024;

/// The most WebVTT tags open at once in a cue whose markup is kept.
pub const MAX_OPEN_TAGS: usize = 32;

/// The most distinct classes the open tags of a cue whose markup is kept give at once.
pub const MAX_HELD_CLASSES: usize = 64;

/// Cues read from a subtitle file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportedCaptions {
    /// Cues in file order, sorted by start time. They may overlap; see [`caption_rows`].
    pub cues: Vec<CaptionCue>,
    /// Blocks that were not cues and could not be read as one.
    pub skipped: usize,
    /// Markup read but not kept.
    pub notes: ImportNotes,
}

/// What an import read but could not keep, counted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportNotes {
    /// Voices after a cue's first, whose names are not its speaker. Their text still takes the
    /// class `voice`.
    pub extra_voices: usize,
    /// Inline timestamps out of order or outside their cue, which WebVTT renderers ignore.
    pub dropped_timestamps: usize,
    /// Class names a run cannot carry (not `A-Z a-z 0-9 _ -`, longer than 64 bytes, or past
    /// [`MAX_RUN_CLASSES`]).
    pub dropped_classes: usize,
    /// Cues imported as plain text because their markup had more than [`MAX_TEXT_RUNS`] runs,
    /// 32 tags open or 64 classes held.
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
    /// The cues need more caption tracks than a project holds: too many overlap at once, or there
    /// are too many to fit [`MAX_CLIPS_PER_TRACK`] to a track.
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
/// Returns [`CaptionImportError::FileTooLarge`] or [`CaptionImportError::TooManyCues`] for a
/// file past the import limits.
pub fn parse_srt(text: &str) -> Result<ImportedCaptions, CaptionImportError> {
    check_file_size(text)?;
    let mut imported = ImportedCaptions::default();
    for block in Blocks::new(text) {
        let Some(block) = block else {
            imported.skipped += 1;
            continue;
        };
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
        let range = timing(first);
        let marked = range.map(|_| srt_markup(&body));
        imported.read(range, marked)?;
    }
    imported.cues.sort_by_key(|cue| cue.timeline_range.start());
    Ok(imported)
}

/// Reads WebVTT (`.vtt`) text. `NOTE`, `STYLE`, and `REGION` blocks are ignored.
///
/// # Errors
/// Returns [`CaptionImportError::MissingWebVttHeader`] unless the text begins with `WEBVTT`,
/// and [`CaptionImportError::FileTooLarge`] or [`CaptionImportError::TooManyCues`] for a
/// file past the import limits.
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
    let mut blocks = Blocks::new(text);
    // The header is the first block up to a timing line; as in the WebVTT parser, a cue may
    // follow it without a blank line. An oversized header holds no cue that is read.
    let header = blocks.next().flatten().unwrap_or_default();
    let cue_under_header = header
        .iter()
        .position(|line| line.contains("-->"))
        .map(|start| Some(header[start..].to_vec()));
    for block in cue_under_header.into_iter().chain(blocks) {
        let Some(block) = block else {
            imported.skipped += 1;
            continue;
        };
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
        let range = timing(timing_line);
        let marked = range.map(|range| vtt_markup(&body.join("\n"), range));
        imported.read(range, marked)?;
    }
    imported.cues.sort_by_key(|cue| cue.timeline_range.start());
    Ok(imported)
}

impl ImportedCaptions {
    /// Keeps a block's cue, or counts it skipped when its timing or text is invalid. `marked` is
    /// the cue's text with the speaker and runs its markup gives.
    fn read(
        &mut self,
        range: Option<TimeRange>,
        marked: Option<Marked>,
    ) -> Result<(), CaptionImportError> {
        let (Some(range), Some(marked)) = (range, marked) else {
            self.skipped += 1;
            return Ok(());
        };
        let Some(text) = cue_text(&marked.text) else {
            self.skipped += 1;
            return Ok(());
        };
        if self.cues.len() == MAX_CAPTION_FILE_CUES {
            return Err(CaptionImportError::TooManyCues {
                maximum: MAX_CAPTION_FILE_CUES,
            });
        }
        let mut cue = CaptionCue::new(range, text);
        let notes = &mut self.notes;
        notes.extra_voices += marked.notes.extra_voices;
        notes.dropped_timestamps += marked.notes.dropped_timestamps;
        notes.dropped_classes += marked.notes.dropped_classes;
        notes.flattened += marked.notes.flattened;
        cue.speaker = marked.speaker;
        if let Some(runs) = marked.runs.filter(|runs| !runs.is_empty()) {
            cue.text = TextContent::Runs(runs);
        }
        self.cues.push(cue);
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

/// Splits cues onto rows, one caption track each. Every cue goes to the first row with room
/// (a track holds [`MAX_CLIPS_PER_TRACK`]) whose last cue has ended by its start.
/// O(n log n) in the cues, however many overlap.
///
/// # Errors
/// Returns [`CaptionImportError::TooManyRows`] for cues that need more rows than a project
/// holds tracks.
pub fn caption_rows(cues: &[CaptionCue]) -> Result<Vec<Vec<CaptionCue>>, CaptionImportError> {
    let mut sorted = cues.to_vec();
    sorted.sort_by_key(|cue| cue.timeline_range.start());
    let mut rows: Vec<Vec<CaptionCue>> = Vec::new();
    // Rows with room, split into those whose last cue has ended by the cue being placed and the
    // others keyed by when theirs ends. A full row is in neither.
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

/// Blank-line-separated blocks of lines, one at a time. `None` stands for a block over
/// [`MAX_CUE_BLOCK_BYTES`] or [`MAX_CUE_BLOCK_LINES`], whose lines are not kept.
struct Blocks<'a> {
    lines: Lines<'a>,
}

impl<'a> Blocks<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            lines: Lines {
                text: text.strip_prefix('\u{feff}').unwrap_or(text),
                at: 0,
                pending_blanks: 0,
                done: false,
            },
        }
    }
}

impl<'a> Iterator for Blocks<'a> {
    type Item = Option<Vec<&'a str>>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut block = Vec::new();
        let mut bytes = 0_usize;
        let mut oversized = false;
        for line in self.lines.by_ref() {
            if line.trim().is_empty() {
                if oversized || !block.is_empty() {
                    break;
                }
                continue;
            }
            if oversized {
                continue;
            }
            bytes += line.len() + 1;
            if bytes > MAX_CUE_BLOCK_BYTES || block.len() == MAX_CUE_BLOCK_LINES {
                oversized = true;
                block = Vec::new();
                continue;
            }
            block.push(line);
        }
        if oversized {
            Some(None)
        } else {
            (!block.is_empty()).then_some(Some(block))
        }
    }
}

/// The lines of a text. LF, CR or CRLF ends a line, as does a run of CRs followed by LF. A CR
/// run without a following LF ends a line per CR.
struct Lines<'a> {
    text: &'a str,
    at: usize,
    /// Empty lines still owed by a CR run.
    pending_blanks: usize,
    done: bool,
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.pending_blanks > 0 {
            self.pending_blanks -= 1;
            return Some("");
        }
        if self.done {
            return None;
        }
        let bytes = self.text.as_bytes();
        let start = self.at;
        let Some(end) = bytes[start..]
            .iter()
            .position(|&byte| matches!(byte, b'\n' | b'\r'))
            .map(|offset| start + offset)
        else {
            self.done = true;
            return Some(&self.text[start..]);
        };
        if bytes[end] == b'\n' {
            self.at = end + 1;
        } else {
            let returns = bytes[end..]
                .iter()
                .take_while(|&&byte| byte == b'\r')
                .count();
            self.at = end + returns;
            if bytes.get(self.at) == Some(&b'\n') {
                self.at += 1;
            } else {
                self.pending_blanks = returns - 1;
            }
        }
        Some(&self.text[start..end])
    }
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

/// `[hh:]mm:ss[,.]mmm`, hours any number of digits, up to
/// [`alpha2_time::MAX_SERIALIZED_TICKS`] (a later cue could not be saved).
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

/// Cue text with surrounding blank lines trimmed, or `None` when empty or too long.
fn cue_text(text: &str) -> Option<String> {
    let text = text.trim_matches('\n').to_owned();
    (!text.trim().is_empty() && text.len() <= MAX_TEXT_CONTENT_BYTES).then_some(text)
}

/// Decodes WebVTT's named and numeric character references; unknown ones stay as written.
///
/// A reference is letters, digits, `#` and `+` closed by `;`, so each `&` looks only that
/// far and the text is read in linear time.
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

/// A cue's text and what its markup says: its speaker, its runs when any carries a class or a
/// time (else the plain text stands), and what was dropped.
struct Marked {
    text: String,
    speaker: Option<String>,
    runs: Option<Vec<TextRun>>,
    notes: ImportNotes,
}

/// Builds a cue's text and runs from text pieces, each with its open classes and time segment.
/// Markup stops being tracked (the cue is imported as plain text) past [`MAX_TEXT_RUNS`] pieces
/// beyond what trimming can remove.
struct Runs {
    text: String,
    pieces: Vec<(String, Vec<String>, usize)>,
    tracking: bool,
    notes: ImportNotes,
}

impl Runs {
    fn new() -> Self {
        Self {
            text: String::new(),
            pieces: Vec::new(),
            tracking: true,
            notes: ImportNotes::default(),
        }
    }

    /// Stops tracking markup: the cue is imported as plain text.
    fn flatten(&mut self) {
        if self.tracking {
            self.tracking = false;
            self.pieces = Vec::new();
            self.notes.flattened += 1;
        }
    }

    fn push(&mut self, text: &str, classes: &[String], segment: usize) {
        self.text.push_str(text);
        if !self.tracking {
            return;
        }
        // Leading line feeds are trimmed from the runs as `cue_text` trims them from the text.
        let text = if self.pieces.is_empty() {
            text.trim_start_matches('\n')
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        match self.pieces.last_mut() {
            Some((held, open, at)) if open.as_slice() == classes && *at == segment => {
                held.push_str(text);
            }
            _ => {
                // Two more than the limit: trailing line feeds can still remove one.
                if self.pieces.len() > MAX_TEXT_RUNS + 1 {
                    self.flatten();
                    return;
                }
                self.pieces
                    .push((text.to_owned(), classes.to_vec(), segment));
            }
        }
    }

    /// The text and runs, timed by `bounds` (each segment's start, then the end, in ticks from
    /// the cue's start), with trailing line feeds trimmed as for plain cue text. No runs when
    /// none carries a class or a time.
    fn finish(mut self, speaker: Option<String>, bounds: &[i64]) -> Marked {
        while let Some(last) = self.pieces.last_mut() {
            let kept = last.0.trim_end_matches('\n').len();
            last.0.truncate(kept);
            if last.0.is_empty() {
                self.pieces.pop();
            } else {
                break;
            }
        }
        if self.pieces.len() > MAX_TEXT_RUNS {
            self.flatten();
        }
        let timed = bounds.len() > 2;
        let runs = (self.tracking
            && (timed
                || self
                    .pieces
                    .iter()
                    .any(|(_, classes, _)| !classes.is_empty())))
        .then(|| {
            self.pieces
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
                .collect()
        });
        Marked {
            text: self.text,
            speaker,
            runs,
            notes: self.notes,
        }
    }
}

/// The classes a tag's dot-separated `names` give; ones a run cannot carry are counted and
/// dropped.
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

/// A WebVTT cue body's text and markup for a cue over `range`: voices, classes, bold, italic and
/// underline as run classes, and inline timestamps splitting the runs. A `<` with no `>` after it
/// ends the cue text, as the WebVTT cue text tokenizer does. Linear in the body: each open tag
/// records how many classes it added, and closing it truncates them.
fn vtt_markup(body: &str, range: TimeRange) -> Marked {
    let start = range.start().ticks();
    let duration = range.duration().map_or(0, MediaTime::ticks);
    let mut runs = Runs::new();
    let mut speaker = None;
    // The tags open, each with the length of `classes` before it opened, innermost last.
    let mut open: Vec<(&str, usize)> = Vec::new();
    // The distinct classes the open tags give, in order.
    let mut classes: Vec<String> = Vec::new();
    let mut bounds = vec![0_i64];
    let mut rest = body;
    loop {
        let Some(at) = rest.find('<') else {
            runs.push(&decode_references(rest), &classes, bounds.len() - 1);
            break;
        };
        runs.push(&decode_references(&rest[..at]), &classes, bounds.len() - 1);
        let Some(end) = rest[at..].find('>') else {
            break;
        };
        let tag = &rest[at + 1..at + end];
        rest = &rest[at + end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            if let Some(index) = open.iter().rposition(|(open, _)| *open == name) {
                classes.truncate(open[index].1);
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
                    if runs.tracking {
                        bounds.push(time.ticks() - start);
                    }
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
        if !runs.tracking {
            continue;
        }
        if open.len() == MAX_OPEN_TAGS {
            runs.flatten();
            continue;
        }
        open.push((name, classes.len()));
        for class in given {
            if !classes.contains(&class) {
                if classes.len() == MAX_HELD_CLASSES {
                    runs.flatten();
                    break;
                }
                classes.push(class);
            }
        }
    }
    bounds.push(duration);
    let speaker = speaker.filter(|speaker| speaker.len() <= crate::MAX_CAPTION_NAME_BYTES);
    runs.finish(speaker, &bounds)
}

/// An SRT cue body's markup: `<b>`, `<i>` and `<u>` as run classes; other tags and override
/// blocks dropped.
fn srt_markup(body: &[&str]) -> Marked {
    let mut runs = Runs::new();
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
    runs.finish(None, &[0, 0])
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
