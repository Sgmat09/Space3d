/// Double-buffered event queue. Events live for two frames so every system
/// gets a chance to read them regardless of ordering. Store as a resource.
pub struct Events<T> {
    a: Vec<(u64, T)>,
    b: Vec<(u64, T)>,
    next_id: u64,
}

impl<T> Default for Events<T> {
    fn default() -> Self {
        Self { a: Vec::new(), b: Vec::new(), next_id: 0 }
    }
}

impl<T> Events<T> {
    pub fn send(&mut self, event: T) {
        self.b.push((self.next_id, event));
        self.next_id += 1;
    }

    /// Swap buffers; called once per frame by [`crate::App`] for registered types.
    pub fn update(&mut self) {
        std::mem::swap(&mut self.a, &mut self.b);
        self.b.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.a.is_empty() && self.b.is_empty()
    }

    /// Iterate all live events (oldest first) without tracking read position.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.a.iter().chain(self.b.iter()).map(|(_, e)| e)
    }

    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.a.drain(..).chain(self.b.drain(..)).map(|(_, e)| e)
    }
}

/// Cursor that yields each event exactly once.
#[derive(Default, Clone, Copy)]
pub struct EventReader {
    last: u64,
}

impl EventReader {
    pub fn read<'a, T>(&mut self, events: &'a Events<T>) -> impl Iterator<Item = &'a T> {
        let from = self.last;
        self.last = events.next_id;
        events.a.iter().chain(events.b.iter()).filter(move |(id, _)| *id >= from).map(|(_, e)| e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reader_sees_each_once_and_events_expire() {
        let mut ev = Events::default();
        let mut r = EventReader::default();
        ev.send(1);
        ev.send(2);
        assert_eq!(r.read(&ev).copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(r.read(&ev).count(), 0);
        ev.update();
        ev.send(3);
        assert_eq!(r.read(&ev).copied().collect::<Vec<_>>(), vec![3]);
        ev.update();
        ev.update();
        assert!(ev.is_empty());
    }
}
