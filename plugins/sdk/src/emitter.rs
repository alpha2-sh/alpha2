use alpha2_plugin_abi::records::{Record, encode};

/// Where a step's values go. They are sent to the host, which validates them as the session does,
/// when the step returns.
#[derive(Debug, Default)]
pub struct Emitter {
    outputs: Vec<String>,
    pub(crate) records: Vec<u8>,
    pub(crate) unknown: Option<String>,
}

impl Emitter {
    pub(crate) fn new(outputs: Vec<String>) -> Self {
        Self {
            outputs,
            ..Self::default()
        }
    }

    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.unknown = None;
    }

    fn index(&mut self, output: &str) -> Option<u16> {
        let index = self
            .outputs
            .iter()
            .position(|name| name == output)
            .and_then(|index| u16::try_from(index).ok());
        if index.is_none() && self.unknown.is_none() {
            self.unknown = Some(output.to_owned());
        }
        index
    }

    /// A sample of a `points`, `scalars` or `similarity` output: one value per channel component,
    /// `None` for a channel lost at `time`.
    pub fn sample(&mut self, output: &str, time: i64, values: &[Option<f32>]) {
        if let Some(output) = self.index(output) {
            let values = values.to_vec();
            encode(
                &Record::Sample {
                    output,
                    time,
                    values,
                },
                &mut self.records,
            );
        }
    }

    /// A word of a `words` output.
    pub fn word(&mut self, output: &str, start: i64, end: i64, text: &str, speaker: Option<&str>) {
        if let Some(output) = self.index(output) {
            let record = Record::Word {
                output,
                start,
                end,
                text: text.to_owned(),
                speaker: speaker.map(str::to_owned),
            };
            encode(&record, &mut self.records);
        }
    }

    /// An interval of an `intervals` output.
    pub fn interval(
        &mut self,
        output: &str,
        start: i64,
        end: i64,
        label: Option<&str>,
        score: Option<f32>,
    ) {
        if let Some(output) = self.index(output) {
            let record = Record::Interval {
                output,
                start,
                end,
                label: label.map(str::to_owned),
                score,
            };
            encode(&record, &mut self.records);
        }
    }
}
