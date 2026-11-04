//! Annotation state is independent of AppKit's responder lifecycle.
use crate::coordinates::Point as NSPoint;

use crate::drawing::{Mark, Stamp, Tool};
use std::sync::Arc;

// A screenshot editor should not accumulate unbounded paths if an input device
// is held down indefinitely. These limits are far above ordinary annotations.
const MAX_MARKS: usize = 4096;
const MAX_CAPTURE_POINTS: usize = 262_144;
const MAX_STROKE_POINTS: usize = 32_768;

pub const COLORS: [[f64; 4]; 4] = [
    [1.0, 0.24, 0.20, 1.0],
    [0.84, 1.0, 0.12, 1.0],
    [0.12, 0.78, 1.0, 1.0],
    [1.0, 1.0, 1.0, 1.0],
];

/// Tracks the immutable raster's committed prefix. A single job may be pending;
/// history edits invalidate its generation without starting parallel rebuilds.
#[derive(Debug, Default)]
pub(crate) struct RasterCacheState {
    generation: u64,
    cached_count: usize,
    pending: Option<CacheRequest>,
    failed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CacheRequest {
    generation: u64,
    pub from: usize,
    pub to: usize,
}

impl RasterCacheState {
    pub fn cached_count(&self) -> usize {
        self.cached_count
    }

    pub fn request(&mut self, count: usize) -> Option<CacheRequest> {
        if self.pending.is_some() || self.failed || count <= self.cached_count {
            return None;
        }
        let request = CacheRequest {
            generation: self.generation,
            from: self.cached_count,
            to: count,
        };
        self.pending = Some(request);
        Some(request)
    }

    /// Return true only when the supplied image represents the current history
    /// prefix. Repeated, superseded, or invalidated completions cannot apply.
    pub fn complete(&mut self, request: CacheRequest, success: bool) -> bool {
        if self.pending != Some(request) {
            return false;
        }
        self.pending = None;
        if request.generation != self.generation || request.from != self.cached_count {
            return false;
        }
        if success {
            self.cached_count = request.to;
            true
        } else {
            self.failed = true;
            false
        }
    }

    /// Preserve an already valid prefix when undo removes only uncached marks.
    /// Otherwise the caller must restore the unannotated crop before redrawing.
    pub fn truncate(&mut self, count: usize) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.failed = false;
        if self.cached_count > count {
            self.cached_count = 0;
            true
        } else {
            false
        }
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cached_count = 0;
        self.failed = false;
    }
}

#[derive(Debug)]
pub struct Editor {
    pub tool: Tool,
    pub color_index: usize,
    pub marks: Vec<Mark>,
    pub redo: Vec<Mark>,
    pub current: Option<Mark>,
    pub pixelation: Option<Arc<Stamp>>,
    revision: u64,
    pixelation_revision: u64,
    pixelation_pending: Option<u64>,
    point_limit: usize,
    saturated: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            tool: Tool::Pen,
            color_index: 0,
            marks: Vec::new(),
            redo: Vec::new(),
            current: None,
            pixelation: None,
            revision: 1,
            pixelation_revision: 0,
            pixelation_pending: None,
            point_limit: MAX_STROKE_POINTS,
            saturated: false,
        }
    }
}

impl Editor {
    pub fn begin(&mut self, point: NSPoint, pixel_scale: f64) -> bool {
        if self.tool == Tool::Text || (self.tool.is_pixelation() && !self.pixelation_ready()) {
            return false;
        }
        if !finite(point) || !pixel_scale.is_finite() || pixel_scale <= 0.0 {
            return false;
        }
        let used = self
            .marks
            .iter()
            .map(|mark| mark.points.len())
            .sum::<usize>();
        if self.marks.len() >= MAX_MARKS || used >= MAX_CAPTURE_POINTS - 1 {
            return false;
        }
        self.point_limit = (MAX_CAPTURE_POINTS - used).min(MAX_STROKE_POINTS);
        self.saturated = false;
        self.current = Some(Mark {
            tool: self.tool,
            points: vec![point],
            color: COLORS[self.color_index.min(COLORS.len() - 1)],
            width: if self.tool.is_pixelation() {
                12.0 * pixel_scale
            } else {
                3.0 * pixel_scale
            },
            stamp: self
                .tool
                .is_pixelation()
                .then(|| self.pixelation.clone())
                .flatten(),
        });
        true
    }

    /// Discard subpixel jitter while preserving shape endpoints. Sampling is
    /// measured in displayed points so a zoomed crop does not become jagged.
    pub fn drag(&mut self, point: NSPoint) -> bool {
        if !finite(point) {
            return true;
        }
        let Some(mark) = self.current.as_mut() else {
            return true;
        };
        match mark.tool {
            Tool::Pen | Tool::Highlighter | Tool::PixelateBrush => {
                let Some(last) = mark.points.last() else {
                    return true;
                };
                let tolerance = mark.width / 12.0;
                if (point.x - last.x).hypot(point.y - last.y) >= tolerance {
                    // Reserve the last slot for the exact mouse-up endpoint.
                    if mark.points.len() >= self.point_limit - 1 {
                        self.saturated = true;
                        return false;
                    }
                    mark.points.push(point);
                }
            }
            Tool::Arrow | Tool::Rectangle | Tool::PixelateArea | Tool::Text => {
                if mark.points.len() == 1 {
                    mark.points.push(point);
                } else {
                    mark.points[1] = point;
                }
            }
        }
        true
    }

    pub fn end(&mut self, point: NSPoint) {
        if let (true, Some(mark)) = (finite(point) && !self.saturated, self.current.as_mut()) {
            match mark.tool {
                Tool::Pen | Tool::Highlighter | Tool::PixelateBrush => {
                    if mark.points.last() != Some(&point) {
                        mark.points.push(point);
                    }
                }
                Tool::Arrow | Tool::Rectangle | Tool::PixelateArea | Tool::Text => {
                    if mark.points.len() == 1 {
                        mark.points.push(point);
                    } else {
                        mark.points[1] = point;
                    }
                }
            }
        }
        self.commit_current();
    }

    pub fn commit_current(&mut self) {
        if let Some(mut mark) = self.current.take() {
            if mark.tool.is_pixelation()
                && let Some(stamp) = mark.stamp.as_ref()
                && let Ok(patch) = stamp.patch(&mark)
            {
                mark.stamp = Some(patch);
            }
            if !mark.tool.is_pixelation() {
                self.revision = self.revision.wrapping_add(1);
            }
            self.marks.push(mark);
            self.redo.clear();
        }
    }
    pub fn pixelation_ready(&self) -> bool {
        self.pixelation.is_some() && self.pixelation_revision == self.revision
    }
    pub fn pixelation_job(&mut self) -> Option<(u64, Vec<Mark>)> {
        if self.pixelation_ready() || self.pixelation_pending.is_some() {
            return None;
        }
        self.pixelation_pending = Some(self.revision);
        Some((self.revision, self.marks.clone()))
    }
    /// Only the newest history can supply a source. A stale completion releases
    /// the one worker slot and requests another build if the tool still needs it.
    pub fn complete_pixelation(&mut self, revision: u64, result: Option<Arc<Stamp>>) -> bool {
        if self.pixelation_pending != Some(revision) {
            return false;
        }
        self.pixelation_pending = None;
        if revision != self.revision {
            return true;
        }
        if let Some(stamp) = result {
            self.pixelation = Some(stamp);
            self.pixelation_revision = revision;
        }
        false
    }
    pub fn insert_text(&mut self, point: NSPoint, stamp: Arc<Stamp>) -> bool {
        let bytes: usize = self
            .marks
            .iter()
            .filter(|m| m.tool == Tool::Text)
            .filter_map(|m| m.stamp.as_ref())
            .map(|s| s.pixels.width() * s.pixels.height() * 4)
            .sum();
        if !finite(point)
            || self.marks.len() + usize::from(self.current.is_some()) >= MAX_MARKS
            || bytes + stamp.pixels.width() * stamp.pixels.height() * 4 > 64 * 1024 * 1024
        {
            return false;
        }
        self.commit_current();
        self.marks.push(Mark {
            tool: Tool::Text,
            points: vec![point],
            color: COLORS[self.color_index.min(COLORS.len() - 1)],
            width: 1.,
            stamp: Some(stamp),
        });
        self.revision = self.revision.wrapping_add(1);
        self.redo.clear();
        true
    }

    pub fn undo(&mut self) {
        if self.current.take().is_some() {
            return;
        }
        if let Some(mark) = self.marks.pop() {
            self.revision = self.revision.wrapping_add(1);
            self.redo.push(mark);
        }
    }

    pub fn redo(&mut self) {
        if self.current.is_some() {
            return;
        }
        if let Some(mark) = self.redo.pop() {
            self.revision = self.revision.wrapping_add(1);
            self.marks.push(mark);
        }
    }
}

fn finite(point: NSPoint) -> bool {
    point.x.is_finite() && point.y.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f64, y: f64) -> NSPoint {
        NSPoint::new(x, y)
    }
    #[test]
    fn pixelation_uses_latest_history_and_commits_only_its_patch() {
        let source = crate::portable_image::synthetic(96, 80).unwrap();
        let mut e = Editor::default();
        let (old, _) = e.pixelation_job().unwrap();
        assert!(e.pixelation_job().is_none());
        e.begin(point(12., 12.), 1.);
        e.end(point(60., 40.));
        let stale = crate::portable_image::pixelated(&source, 8).unwrap();
        assert!(e.complete_pixelation(old, Some(stale)));
        assert!(!e.pixelation_ready());
        let (revision, marks) = e.pixelation_job().unwrap();
        assert_eq!(marks.len(), 1);
        let visible = crate::portable_image::render(&source, &marks).unwrap();
        let prepared = crate::portable_image::pixelated(&visible, 8).unwrap();
        e.complete_pixelation(revision, Some(prepared));
        assert!(e.pixelation_ready());
        e.tool = Tool::PixelateArea;
        assert!(e.begin(point(18., 16.), 1.));
        e.drag(point(38., 34.));
        let full = e.current.clone().unwrap();
        e.end(point(38., 34.));
        let patch = e.marks.last().unwrap().stamp.as_ref().unwrap();
        assert!(patch.pixels.width() < source.width());
        assert!(patch.pixels.height() < source.height());
        assert!(patch.origin.x > 0.);
        assert_eq!(
            crate::portable_image::render(&visible, &[full])
                .unwrap()
                .pixels(),
            crate::portable_image::render(&source, &e.marks)
                .unwrap()
                .pixels()
        );
        assert!(e.pixelation_ready());
        e.undo();
        assert!(!e.pixelation_ready());
        e.redo();
        assert!(!e.pixelation_ready());
    }

    #[test]
    fn click_creates_a_drawable_dot() {
        let mut e = Editor::default();
        e.begin(point(20.0, 25.0), 2.0);
        e.end(point(20.0, 25.0));
        assert_eq!(e.marks.len(), 1);
        assert_eq!(e.marks[0].points.len(), 1);
        assert_eq!(e.marks[0].width, 6.0);
    }

    #[test]
    fn endpoint_is_preserved_even_below_sampling_threshold() {
        let mut e = Editor::default();
        e.begin(point(10.0, 10.0), 1.0);
        e.drag(point(10.05, 10.05));
        assert_eq!(e.current.as_ref().unwrap().points.len(), 1);
        e.end(point(10.1, 10.1));
        assert_eq!(e.marks[0].points.last(), Some(&point(10.1, 10.1)));
    }

    #[test]
    fn shape_keeps_only_first_and_latest_points() {
        let mut e = Editor {
            tool: Tool::Arrow,
            ..Editor::default()
        };
        e.begin(point(10.0, 20.0), 1.0);
        for i in 0..100 {
            e.drag(point(i as f64, 80.0));
        }
        e.end(point(110.0, 100.0));
        assert_eq!(
            e.marks[0].points,
            vec![point(10.0, 20.0), point(110.0, 100.0)]
        );
    }

    #[test]
    fn undo_and_redo_preserve_order_and_color() {
        let mut e = Editor::default();
        for i in 0..3 {
            e.color_index = i;
            e.begin(point(i as f64, 20.0), 1.0);
            e.end(point(i as f64, 25.0));
        }
        e.undo();
        e.undo();
        assert_eq!(e.marks.len(), 1);
        e.redo();
        e.redo();
        assert_eq!(e.marks.len(), 3);
        assert_eq!(e.marks[1].color, COLORS[1]);
        assert_eq!(e.marks[2].color, COLORS[2]);
    }

    #[test]
    fn a_new_stroke_invalidates_redo_only_when_committed() {
        let mut e = Editor::default();
        e.begin(point(1.0, 1.0), 1.0);
        e.end(point(2.0, 2.0));
        e.undo();
        e.begin(point(3.0, 3.0), 1.0);
        assert_eq!(e.redo.len(), 1);
        e.end(point(4.0, 4.0));
        assert!(e.redo.is_empty());
    }

    #[test]
    fn undo_cancels_unfinished_stroke_before_history() {
        let mut e = Editor::default();
        e.begin(point(1.0, 1.0), 1.0);
        e.end(point(2.0, 2.0));
        e.begin(point(3.0, 3.0), 1.0);
        e.undo();
        assert!(e.current.is_none());
        assert_eq!(e.marks.len(), 1);
        assert!(e.redo.is_empty());
    }

    #[test]
    fn repeated_commit_does_not_duplicate_strokes() {
        let mut e = Editor::default();
        e.begin(point(1.0, 1.0), 1.0);
        e.commit_current();
        e.commit_current();
        assert_eq!(e.marks.len(), 1);
    }

    #[test]
    fn nonfinite_input_cannot_poison_a_path() {
        let mut e = Editor::default();
        e.begin(point(f64::NAN, 1.0), 1.0);
        assert!(e.current.is_none());
        e.begin(point(1.0, 1.0), f64::INFINITY);
        assert!(e.current.is_none());
        e.begin(point(1.0, 1.0), 1.0);
        e.drag(point(f64::INFINITY, 1.0));
        e.end(point(2.0, 3.0));
        assert!(e.marks[0].points.iter().all(|p| finite(*p)));
    }

    #[test]
    fn indefinitely_dragging_cannot_grow_a_stroke_without_bound() {
        let mut e = Editor::default();
        assert!(e.begin(point(0.0, 0.0), 1.0));
        for i in 1..MAX_STROKE_POINTS + 100 {
            e.drag(point(i as f64, 1.0));
        }
        assert!(e.saturated);
        e.end(point(1_000_000.0, 1.0));
        assert!(e.marks[0].points.len() <= MAX_STROKE_POINTS);
        // Exhaustion must not add a long, unexpected line to the mouse-up point.
        assert!(e.marks[0].points.last().unwrap().x < MAX_STROKE_POINTS as f64);
        assert!(e.begin(point(1.0, 1.0), 1.0));
        assert!(!e.saturated);
    }

    #[test]
    fn a_full_canvas_remains_copyable_and_undo_frees_capacity() {
        let mut e = Editor::default();
        for _ in 0..MAX_MARKS {
            assert!(e.begin(point(0.0, 0.0), 1.0));
            e.end(point(0.0, 0.0));
        }
        assert!(!e.begin(point(1.0, 1.0), 1.0));
        assert_eq!(e.marks.len(), MAX_MARKS);
        e.undo();
        assert!(e.begin(point(1.0, 1.0), 1.0));
    }

    #[test]
    fn raster_cache_batches_only_new_commits_and_allows_one_job() {
        let mut cache = RasterCacheState::default();
        assert!(cache.request(0).is_none());
        let first = cache.request(1).unwrap();
        assert_eq!((first.from, first.to), (0, 1));
        assert!(cache.request(2).is_none());
        assert!(cache.complete(first, true));
        assert!(!cache.complete(first, true));
        let next = cache.request(3).unwrap();
        assert_eq!((next.from, next.to), (1, 3));
        assert!(cache.complete(next, true));
        assert_eq!(cache.cached_count(), 3);
        assert!(cache.request(3).is_none());
    }

    #[test]
    fn raster_cache_undo_discards_pixels_from_removed_marks() {
        let mut cache = RasterCacheState::default();
        let initial = cache.request(3).unwrap();
        assert!(cache.complete(initial, true));
        assert!(cache.truncate(2));
        assert_eq!(cache.cached_count(), 0);
        let rebuild = cache.request(2).unwrap();
        assert_eq!((rebuild.from, rebuild.to), (0, 2));
        assert!(cache.complete(rebuild, true));
        let redo = cache.request(3).unwrap();
        assert_eq!((redo.from, redo.to), (2, 3));
    }

    #[test]
    fn raster_cache_rejects_same_length_replacement_from_old_worker() {
        let mut cache = RasterCacheState::default();
        let obsolete = cache.request(1).unwrap();
        assert!(!cache.truncate(0));
        assert!(cache.request(1).is_none()); // old job still owns its work buffers
        assert!(!cache.complete(obsolete, true));
        assert_eq!(cache.cached_count(), 0);
        let replacement = cache.request(1).unwrap();
        assert_ne!(obsolete, replacement);
        assert!(cache.complete(replacement, true));
    }

    #[test]
    fn raster_cache_undo_uncached_tail_preserves_valid_pixels() {
        let mut cache = RasterCacheState::default();
        let initial = cache.request(2).unwrap();
        assert!(cache.complete(initial, true));
        let obsolete = cache.request(3).unwrap();
        assert!(!cache.truncate(2));
        assert!(!cache.complete(obsolete, true));
        assert_eq!(cache.cached_count(), 2);
        assert!(cache.request(2).is_none());
    }

    #[test]
    fn raster_cache_allocation_failure_does_not_retry_in_a_loop() {
        let mut cache = RasterCacheState::default();
        let request = cache.request(1).unwrap();
        assert!(!cache.complete(request, false));
        assert_eq!(cache.cached_count(), 0);
        assert!(cache.request(1).is_none());
        cache.reset();
        assert!(cache.request(1).is_some());
    }

    #[test]
    fn raster_cache_remains_a_correct_prefix_under_interleaved_history_and_workers() {
        let mut cache = RasterCacheState::default();
        let mut history = Vec::new();
        let mut redo = Vec::new();
        let mut raster = Vec::new();
        let mut pending: Option<(CacheRequest, Vec<usize>)> = None;
        let mut random = 0x51a7_2026_u32;
        for id in 0..5000 {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            match random % 7 {
                0 => {
                    history.push(id);
                    redo.clear();
                }
                1 => {
                    if let Some(mark) = history.pop() {
                        redo.push(mark);
                        if cache.truncate(history.len()) {
                            raster.clear();
                        }
                    }
                }
                2 => {
                    if let Some(mark) = redo.pop() {
                        history.push(mark);
                    }
                }
                3 => {
                    if let Some(job) = cache.request(history.len()) {
                        assert!(pending.is_none());
                        let mut result = raster.clone();
                        result.extend_from_slice(&history[job.from..job.to]);
                        pending = Some((job, result));
                    }
                }
                4 | 5 => {
                    if let Some((job, result)) = pending.take()
                        && cache.complete(job, random % 7 == 4)
                    {
                        raster = result;
                    }
                }
                6 => {
                    cache.reset();
                    raster.clear();
                }
                _ => unreachable!(),
            }
            assert!(cache.cached_count() <= history.len());
            assert_eq!(raster, history[..cache.cached_count()], "operation {id}");
        }
    }

    #[test]
    fn incremental_raster_matches_export_without_double_painting() {
        use crate::imaging;
        let source = imaging::synthetic(96, 80).unwrap();
        let marks = vec![
            Mark {
                tool: Tool::Rectangle,
                points: vec![point(12.0, 10.0), point(70.0, 55.0)],
                color: COLORS[0],
                width: 3.0,
                stamp: None,
            },
            Mark {
                tool: Tool::Highlighter,
                points: vec![point(10.0, 30.0), point(80.0, 30.0)],
                color: COLORS[1],
                width: 3.0,
                stamp: None,
            },
            Mark {
                tool: Tool::Highlighter,
                points: vec![point(50.0, 12.0), point(50.0, 65.0)],
                color: COLORS[2],
                width: 3.0,
                stamp: None,
            },
            Mark {
                tool: Tool::Pen,
                points: vec![point(5.0, 60.0), point(65.0, 5.0)],
                color: COLORS[3],
                width: 2.0,
                stamp: None,
            },
        ];
        let mut cache = RasterCacheState::default();
        let mut raster = source.clone();
        for count in 1..=marks.len() {
            let job = cache.request(count).unwrap();
            raster = imaging::render(&raster, &marks[job.from..job.to]).unwrap();
            assert!(cache.complete(job, true));
            assert!(cache.request(count).is_none());
        }
        let exported = imaging::render(&source, &marks).unwrap();
        assert_eq!(
            imaging::encode(&raster).unwrap().png,
            imaging::encode(&exported).unwrap().png
        );
        // Undo cannot paint over the old raster: rebuild from the original.
        assert!(cache.truncate(2));
        let undo = cache.request(2).unwrap();
        raster = imaging::render(&source, &marks[undo.from..undo.to]).unwrap();
        assert!(cache.complete(undo, true));
        let redone = cache.request(3).unwrap();
        raster = imaging::render(&raster, &marks[redone.from..redone.to]).unwrap();
        assert!(cache.complete(redone, true));
        let expected = imaging::render(&source, &marks[..3]).unwrap();
        assert_eq!(
            imaging::encode(&raster).unwrap().png,
            imaging::encode(&expected).unwrap().png
        );
    }
}
