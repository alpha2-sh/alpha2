//! Joins the words of overlapping windows into one sequence without repeating the overlap.

/// A word, its times in samples on the input's clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub start: i64,
    pub end: i64,
    pub text: String,
}

impl Word {
    fn middle(&self) -> i64 {
        self.start + (self.end - self.start) / 2
    }
}

const SAME_WORD_SAMPLES: i64 = 8_000;
const EDGE_SAMPLES: i64 = 16_000;

fn key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// How many words of `earlier` to keep, and the index in `later` of the first word to keep.
#[must_use]
pub fn join(earlier: &[Word], later: &[Word], overlap: (i64, i64)) -> (usize, usize) {
    let (from, to) = overlap;
    let middle = from + (to - from) / 2;
    let mut best: Option<(i64, usize, usize)> = None;
    for (i, word) in earlier.iter().enumerate() {
        let at = word.middle();
        if at < from || at >= to - EDGE_SAMPLES {
            continue;
        }
        let wanted = key(&word.text);
        if wanted.is_empty() {
            continue;
        }
        for (j, other) in later.iter().enumerate() {
            let other_at = other.middle();
            if other_at < from + EDGE_SAMPLES || other_at >= to {
                continue;
            }
            if (other_at - at).abs() > SAME_WORD_SAMPLES || key(&other.text) != wanted {
                continue;
            }
            let distance = (at - middle).abs();
            if best.is_none_or(|(known, ..)| distance < known) {
                best = Some((distance, i, j));
            }
        }
    }
    match best {
        Some((_, i, j)) => (i + 1, j + 1),
        None => (
            earlier.partition_point(|word| word.middle() < middle),
            later.partition_point(|word| word.middle() < middle),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 16_000;

    fn words(list: &[(&str, f64)]) -> Vec<Word> {
        list.iter()
            .map(|&(text, at)| {
                #[expect(clippy::cast_possible_truncation, reason = "times of a few seconds")]
                let start = (at * 16_000.0) as i64;
                Word {
                    start,
                    end: start + SECOND / 4,
                    text: text.to_owned(),
                }
            })
            .collect()
    }

    fn joined(earlier: &[Word], later: &[Word], overlap: (i64, i64)) -> Vec<String> {
        let (keep, from) = join(earlier, later, overlap);
        earlier[..keep]
            .iter()
            .chain(&later[from..])
            .map(|word| word.text.clone())
            .collect()
    }

    #[test]
    fn a_word_both_windows_heard_joins_them_once() {
        let earlier = words(&[
            ("the", 20.0),
            ("art", 25.5),
            ("of", 26.5),
            ("war,", 27.5),
            ("is", 29.1),
        ]);
        let later = words(&[
            ("t", 25.0),
            ("of", 26.55),
            ("War", 27.45),
            ("is", 29.2),
            ("vital", 30.0),
        ]);
        assert_eq!(
            joined(&earlier, &later, (25 * SECOND, 30 * SECOND)),
            ["the", "art", "of", "war,", "is", "vital"]
        );
    }

    #[test]
    fn the_shared_word_nearest_the_middle_of_the_overlap_joins() {
        let earlier = words(&[("a", 25.6), ("b", 26.6), ("c", 27.6), ("d", 28.6)]);
        let later = words(&[("a", 25.7), ("b", 26.6), ("x", 27.6), ("d", 28.6)]);
        assert_eq!(
            join(&earlier, &later, (25 * SECOND, 30 * SECOND)),
            (2, 2),
            "b, at 26.6 s, is nearer the middle at 27.5 s than d, at 28.6 s"
        );
    }

    #[test]
    fn a_word_in_the_first_second_of_the_later_window_does_not_join() {
        let earlier = words(&[("a", 25.3), ("b", 27.0)]);
        let later = words(&[("a", 25.3), ("c", 27.0)]);
        assert_eq!(
            joined(&earlier, &later, (25 * SECOND, 30 * SECOND)),
            ["a", "b"],
            "no word joins, so each window keeps its side of 27.5 s"
        );
    }

    #[test]
    fn a_word_in_the_last_second_of_the_earlier_window_does_not_join() {
        let earlier = words(&[("b", 28.0), ("a", 29.3)]);
        let later = words(&[("c", 28.0), ("a", 29.3)]);
        assert_eq!(
            joined(&earlier, &later, (25 * SECOND, 30 * SECOND)),
            ["c", "a"]
        );
    }

    #[test]
    fn without_a_shared_word_each_window_keeps_its_side_of_the_middle() {
        let earlier = words(&[("alpha", 24.0), ("beta", 26.0), ("gamma", 28.0)]);
        let later = words(&[("delta", 26.5), ("epsilon", 28.2), ("zeta", 31.0)]);
        assert_eq!(
            joined(&earlier, &later, (25 * SECOND, 30 * SECOND)),
            ["alpha", "beta", "epsilon", "zeta"]
        );
    }

    #[test]
    fn the_same_text_far_apart_is_not_the_same_word() {
        let earlier = words(&[("the", 26.0), ("end", 27.0)]);
        let later = words(&[("start", 26.2), ("the", 27.0), ("next", 28.0)]);
        assert_eq!(
            joined(&earlier, &later, (25 * SECOND, 30 * SECOND)),
            ["the", "end", "next"],
            "the earlier the at 26 s and the later one at 27 s are a second apart"
        );
    }

    #[test]
    fn punctuation_and_case_do_not_keep_words_apart_but_a_bare_mark_never_joins() {
        assert_eq!(key("War,"), key("war"));
        assert_eq!(key("--"), "");
        let earlier = words(&[("-", 27.0)]);
        let later = words(&[("-", 27.0)]);
        assert_eq!(join(&earlier, &later, (25 * SECOND, 30 * SECOND)), (1, 1));
    }
}
