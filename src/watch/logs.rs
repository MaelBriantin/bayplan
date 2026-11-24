//! How the logs of the selected container are looked at: scrolled back.

use std::ops::Range;

#[derive(Default)]
pub struct LogView {
    /// Lines hidden below the view; 0 follows the end as it grows.
    pub scroll: usize,
}

impl LogView {
    pub fn up(&mut self, lines: usize, total: usize) {
        self.scroll = (self.scroll + lines).min(total.saturating_sub(1));
    }

    pub fn down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// Forgets the place in the logs, as when another container is selected.
    pub fn reset(&mut self) {
        self.scroll = 0;
    }

    /// Lines shown in `height` rows: the last ones, or the ones above what is scrolled away, keeping the
    /// view full when scrolled back to the start.
    pub fn window(&self, total: usize, height: usize) -> Range<usize> {
        let end = total.saturating_sub(self.scroll).max(height.min(total));
        end.saturating_sub(height)..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolls_back_and_keeps_the_view_full() {
        let mut view = LogView::default();
        assert_eq!(view.window(100, 10), 90..100);
        view.up(30, 100);
        assert_eq!(view.window(100, 10), 60..70);
        view.up(500, 100);
        assert_eq!(view.window(100, 10), 0..10);
        view.down(500);
        assert_eq!(view.window(100, 10), 90..100);
        assert_eq!(view.window(4, 10), 0..4);
    }
}
