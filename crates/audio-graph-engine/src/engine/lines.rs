//! Carrying delay rings across a program swap.

use super::*;

/// Move a delay line's contents into a differently sized ring.
///
/// The samples that matter are the most recent ones, so the copy walks
/// backwards from the old head and lands them at the end of the new ring.
/// Anything that no longer fits is the oldest of it, which is the part a
/// shorter line was never going to read again anyway.
///
/// `head` comes in pointing into the old ring and goes out pointing into the
/// new one.
pub(super) fn copy_ring(
    from: &[f32],
    from_len: usize,
    to: &mut [f32],
    to_len: usize,
    head: &mut usize,
) {
    if from_len == 0 || to_len == 0 {
        *head = 0;
        return;
    }
    let keep = from_len.min(to_len);
    for ch in 0..MAX_CHANNELS {
        let (src, dst) = (ch * from_len, ch * to_len);
        for i in 0..keep {
            // `keep` samples ending at the old head, laid down ending at the
            // new one — which is `keep`, since the new ring starts empty.
            let at = (*head + from_len - keep + i) % from_len;
            if src + at < from.len() && dst + i < to.len() {
                to[dst + i] = from[src + at];
            }
        }
    }
    *head = keep % to_len;
}

/// One step of [`reorder`], applied by the caller to every per-line table it keeps.
pub(super) enum Move {
    Swap(usize, usize),
    /// The slot's line is gone.
    Clear(usize),
}

/// Move each line to the index the new program gave its node, contents intact.
///
/// Work out the permutation first, then hand it to `apply` a step at a time.
/// The caller moves the outer `Vec` of a ring rather than its contents, which
/// for an audio line is 96 000 samples a channel, and moves every other
/// per-line table with it. A table left out would describe the ring that used
/// to sit at an index rather than the one sitting there now.
pub(super) fn reorder(
    nodes: &mut [u32],
    order: &mut [usize],
    want: &[u32],
    mut apply: impl FnMut(Move),
) {
    let lines = want.len().min(nodes.len());
    for (i, slot) in order[..lines].iter_mut().enumerate() {
        *slot = nodes
            .iter()
            .position(|&n| n == want[i])
            .unwrap_or(NOT_PRESENT);
    }

    // Move the surviving rings into place first. Clearing as we went would wipe
    // a ring that is still sitting in a slot some later line wants.
    for i in 0..lines {
        let from = order[i];
        // `from` is never below `i`: slots below `i` already hold the rings of
        // earlier lines, whose nodes are all different from this one's.
        if from == NOT_PRESENT || from == i {
            continue;
        }
        apply(Move::Swap(i, from));
        // Whatever was at `i` now sits at `from`; a line still pointing at `i`
        // has to follow it there.
        for slot in order[i + 1..lines].iter_mut() {
            if *slot == i {
                *slot = from;
            }
        }
        order[i] = i;
    }
    // Whatever is left in a new line's slot belonged to a line that is gone.
    for i in 0..lines {
        if order[i] == NOT_PRESENT {
            apply(Move::Clear(i));
        }
        nodes[i] = want[i];
    }
    for node in nodes[lines..].iter_mut() {
        *node = u32::MAX;
    }
}
