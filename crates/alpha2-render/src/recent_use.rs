//! What each of a pool's last few frames had in use at once, so pools keep what frames reuse.

pub(crate) const RECENT_FRAMES: usize = 8;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecentPeaks {
    newest_first: [usize; RECENT_FRAMES],
}

impl RecentPeaks {
    pub(crate) const fn new() -> Self {
        Self {
            newest_first: [0; RECENT_FRAMES],
        }
    }

    pub(crate) fn most(&self) -> usize {
        self.newest_first.iter().copied().max().unwrap_or(0)
    }

    pub(crate) fn raise(&mut self, in_use: usize) {
        self.newest_first[0] = self.newest_first[0].max(in_use);
    }

    pub(crate) fn advance(&mut self, frames: u64, in_use: usize) {
        let shift =
            usize::try_from(frames).map_or(RECENT_FRAMES, |frames| frames.min(RECENT_FRAMES));
        self.newest_first
            .copy_within(..RECENT_FRAMES - shift, shift);
        self.newest_first[..shift].fill(in_use);
    }
}
