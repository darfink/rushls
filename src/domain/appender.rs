/// A caller-owned buffer that a pipeline stage appends its output to.
///
/// Stages write through this rather than returning a collection because the
/// relationship between input and output is not one-to-one: a packet may
/// normalize into several samples or none, and a sample may close both a part
/// and a segment. A return type expresses that only by allocating per call.
///
/// The single method is the point. A stage cannot read, reorder, remove, or
/// clear what is already in the buffer, because there is no way to ask. Several
/// stages append into one buffer before the caller looks at it, and the caller
/// identifies each contribution by position, so a stage able to disturb earlier
/// entries could corrupt output that is not its own. Encoding "append-only" in
/// the type removes that possibility along with the need to defend against it.
///
/// # Why not [`Extend`]
///
/// Because it cannot be used here. `Extend`'s only required method is
/// `extend<T: IntoIterator<Item = A>>`, which is generic, so no vtable can be
/// built for it and the trait is not dyn compatible. Neither way of reaching for
/// it survives:
///
/// - `out: &mut dyn Extend<T>` does not compile.
/// - `fn push<E: Extend<T>>(&mut self, out: &mut E)` compiles, but gives the
///   *stage* a generic method, so `Box<dyn MediaNormalizer>` stops compiling
///   instead — and erasing stages to trait objects is what keeps type parameters
///   out of every signature above them.
///
/// `Extend::extend_one` would be the right shape, but it is unstable and sits on
/// a trait that is not dyn compatible regardless.
///
/// The narrower trait is a better fit anyway. `Extend` is implemented for
/// `HashSet` and `HashMap`, where "extend" neither preserves order nor appends
/// at a known position, and this contract depends on both.
/// `Send` because a buffer is borrowed across the `await` in a batch read, and
/// every stage that writes to one is already `Send` for the same reason.
pub trait Appender<T>: Send {
    fn push(&mut self, item: T);
}

impl<T: Send> Appender<T> for Vec<T> {
    #[inline]
    fn push(&mut self, item: T) {
        Vec::push(self, item);
    }
}

impl<T: Send> Appender<T> for std::collections::VecDeque<T> {
    #[inline]
    fn push(&mut self, item: T) {
        self.push_back(item);
    }
}
