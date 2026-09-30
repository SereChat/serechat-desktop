//! Shelf packing with least-recently-used eviction, shared by the glyph
//! atlas (`text.rs`) and the image atlas (`image.rs`).
//!
//! Items go into rows ("shelves") whose heights are rounded up to a bucket,
//! so an emptied shelf fits other items of the same size class. When the
//! texture is full, the shelf used least recently is emptied and reused,
//! but never one drawn in the current frame: its texels are already
//! referenced by this frame's instances. Only when every shelf is in use
//! does allocation fail, and the caller wipes the atlas and redraws.

/// Shelf heights are multiples of this, in texels.
const BUCKET: u32 = 8;

/// Marks an item that occupies no shelf (a glyph without ink).
pub const NO_SHELF: u16 = u16::MAX;

/// One row of the atlas.
#[derive(Clone, Copy, Debug)]
struct Shelf {
    /// Top edge.
    y: u32,
    /// Height, a multiple of [`BUCKET`].
    h: u32,
    /// Where the next item goes.
    x: u32,
    /// Frame the shelf was last drawn from.
    used: u64,
}

/// A place in the atlas returned by [`Shelves::alloc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Left edge in texels.
    pub x: u32,
    /// Top edge in texels.
    pub y: u32,
    /// Shelf holding it, for [`Shelves::touch`].
    pub shelf: u16,
    /// A shelf emptied to make room: everything in it must be forgotten.
    pub evicted: Option<u16>,
}

/// Allocator for a square texture of `size` texels.
#[derive(Debug)]
pub struct Shelves {
    size: u32,
    rows: Vec<Shelf>,
    /// Top of the unused space below the last shelf.
    bottom: u32,
    frame: u64,
}

impl Shelves {
    /// An empty atlas of `size`×`size` texels.
    #[must_use]
    pub fn new(size: u32) -> Self {
        Self { size, rows: Vec::new(), bottom: 0, frame: 1 }
    }

    /// Starts a frame: shelves touched from now on count as in use.
    pub fn next_frame(&mut self) {
        self.frame += 1;
    }

    /// Records that an item on shelf `index` is drawn this frame.
    pub fn touch(&mut self, index: u16) {
        if let Some(s) = self.rows.get_mut(usize::from(index)) {
            s.used = self.frame;
        }
    }

    /// Finds room for a `w`×`h` item (padding included by the caller).
    /// `None` means every shelf that could hold it is in use this frame.
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<Slot> {
        if w > self.size || h > self.size || self.rows.len() >= usize::from(NO_SHELF) {
            return None;
        }
        let bucket = h.div_ceil(BUCKET).max(1) * BUCKET;
        let fits = |s: &Shelf| s.h == bucket && s.x + w <= self.size;
        let (index, evicted) = if let Some(index) = self.rows.iter().position(fits) {
            (index, None)
        } else if self.bottom + bucket <= self.size {
            self.rows.push(Shelf { y: self.bottom, h: bucket, x: 0, used: 0 });
            self.bottom += bucket;
            (self.rows.len() - 1, None)
        } else {
            // Least recently used shelf that is tall enough, preferring the
            // same size class so tall shelves are not wasted on small items.
            let frame = self.frame;
            let index = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, s)| s.h >= bucket && s.used < frame)
                .min_by_key(|(_, s)| (s.h != bucket, s.used))
                .map(|(i, _)| i)?;
            self.rows[index].x = 0;
            (index, Some(index as u16))
        };
        let row = &mut self.rows[index];
        let slot = Slot { x: row.x, y: row.y, shelf: index as u16, evicted };
        row.x += w;
        row.used = self.frame;
        Some(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_rows_and_evicts_the_least_recently_used() {
        let mut atlas = Shelves::new(32);
        // Four 16×16 items fill a 32×32 atlas in two shelves.
        let a = atlas.alloc(16, 16).unwrap();
        let b = atlas.alloc(16, 16).unwrap();
        assert_eq!((a.shelf, b.shelf, b.x), (0, 0, 16));
        atlas.alloc(16, 16).unwrap();
        atlas.alloc(16, 16).unwrap();
        assert!(atlas.alloc(16, 16).is_none(), "everything is in use this frame");

        atlas.next_frame();
        atlas.touch(0);
        let c = atlas.alloc(16, 16).unwrap();
        assert_eq!((c.shelf, c.evicted, c.x, c.y), (1, Some(1), 0, 16), "the untouched shelf is reused");
        // Items taller than the atlas never fit.
        assert!(atlas.alloc(8, 64).is_none());
    }

    #[test]
    fn heights_round_to_buckets() {
        let mut atlas = Shelves::new(64);
        let a = atlas.alloc(10, 9).unwrap();
        let b = atlas.alloc(10, 14).unwrap();
        assert_eq!(a.shelf, b.shelf, "9 and 14 share the 16 bucket");
        let c = atlas.alloc(10, 17).unwrap();
        assert_eq!((c.shelf, c.y), (1, 16));
    }
}
