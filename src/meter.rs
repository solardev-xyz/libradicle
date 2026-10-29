//! What a bounded walk may still build: [`crate::Bounds`] applied to one
//! call.

use std::io;

use crate::{json, Bounds, Error};

/// Counts the bytes written to it, and keeps none of them.
pub(crate) struct Count(pub(crate) u64);

impl io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The bytes one walk has charged against its budget.
pub(crate) struct Meter<'a> {
    bounds: &'a Bounds,
    used: u64,
    items: u64,
}

impl<'a> Meter<'a> {
    /// A meter for a walk whose result is no array: nothing is charged yet.
    pub(crate) fn new(bounds: &'a Bounds) -> Self {
        Self {
            bounds,
            used: 0,
            items: 0,
        }
    }

    /// A meter for a walk whose result is a JSON array: its brackets are
    /// charged at once.
    pub(crate) fn array(bounds: &'a Bounds) -> Result<Self, Error> {
        let mut meter = Self::new(bounds);
        meter.charge(2)?;
        Ok(meter)
    }

    /// Whether there is a budget to count against at all. An unbounded walk
    /// counts nothing.
    pub(crate) fn counts(&self) -> bool {
        self.bounds.budget != u64::MAX
    }

    /// `Error::Cancelled` once the walk's token is cancelled.
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.bounds.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(())
    }

    /// Charges `bytes`: past the budget, `Error::TooLarge` with what the walk
    /// would have needed.
    pub(crate) fn charge(&mut self, bytes: u64) -> Result<(), Error> {
        self.check()?;
        let used = self.used.saturating_add(bytes);
        if used > self.bounds.budget {
            return Err(Error::TooLarge {
                size: used,
                limit: self.bounds.budget,
            });
        }
        self.used = used;
        Ok(())
    }

    /// Charges one array item: its JSON as `write` writes it, and the comma
    /// before it unless it is the first.
    pub(crate) fn item(
        &mut self,
        write: impl FnOnce(&mut Count) -> json::Written,
    ) -> Result<(), Error> {
        self.check()?;
        if !self.counts() {
            return Ok(());
        }
        let mut count = Count(u64::from(self.items > 0));
        write(&mut count)?;
        self.charge(count.0)?;
        self.items += 1;
        Ok(())
    }

    /// `Error::TooLarge` unless one more array item of `floor` bytes, and the
    /// comma before it unless it is the first, still fits: the check before
    /// an item is loaded, `floor` the least JSON any item takes. Nothing is
    /// charged; [`Meter::item`] charges the item once it is built.
    pub(crate) fn admits(&self, floor: u64) -> Result<(), Error> {
        self.check()?;
        let need = floor.saturating_add(u64::from(self.items > 0));
        if need > self.left() {
            return Err(Error::TooLarge {
                size: self.used.saturating_add(need),
                limit: self.bounds.budget,
            });
        }
        Ok(())
    }

    /// Charges a whole result: its JSON as `write` writes it.
    pub(crate) fn whole(
        &mut self,
        write: impl FnOnce(&mut Count) -> json::Written,
    ) -> Result<(), Error> {
        self.check()?;
        if !self.counts() {
            return Ok(());
        }
        let mut count = Count(0);
        write(&mut count)?;
        self.charge(count.0)
    }

    /// What is left of the budget.
    pub(crate) fn left(&self) -> u64 {
        self.bounds.budget - self.used
    }

    /// The bounds it charges against.
    pub(crate) fn bounds(&self) -> &Bounds {
        self.bounds
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::Meter;
    use crate::{Bounds, CancelToken, Error};

    /// An array item of `bytes` bytes.
    fn item(meter: &mut Meter, bytes: usize) -> Result<(), Error> {
        meter.item(|w| {
            w.write_all(&vec![b'x'; bytes])
                .map_err(serde_json::Error::io)
        })
    }

    // Review of libradicle #8, round 2: an item is admitted while it, at its
    // floor, and the comma before it, fit; the first has no comma.
    #[test]
    fn an_item_is_admitted_while_its_floor_and_comma_fit() {
        let bounds = Bounds::new(CancelToken::new(), 2 + 62 + 1 + 62);
        let mut meter = Meter::array(&bounds).unwrap();
        meter.admits(62).unwrap();
        item(&mut meter, 62).unwrap();
        meter.admits(62).unwrap();
        item(&mut meter, 62).unwrap();
        assert!(matches!(
            meter.admits(0),
            Err(Error::TooLarge {
                size: 128,
                limit: 127
            })
        ));

        let bounds = Bounds::new(CancelToken::new(), 2 + 62 + 62);
        let mut meter = Meter::array(&bounds).unwrap();
        item(&mut meter, 62).unwrap();
        assert!(matches!(
            meter.admits(62),
            Err(Error::TooLarge {
                size: 127,
                limit: 126
            })
        ));
        assert!(matches!(
            Meter::array(&Bounds::new(CancelToken::new(), 2 + 61))
                .unwrap()
                .admits(62),
            Err(Error::TooLarge {
                size: 64,
                limit: 63
            })
        ));
    }
}
