//! Random access inspection of the results of a dataflow analysis.

use std::borrow::Borrow;
use std::cmp::Ordering;

use rustc_index::bit_set::BitSet;
use rustc_middle::mir::{self, BasicBlock, Location};

use super::{Analysis, Direction, Effect, EffectIndex, Results};

/// A `ResultsCursor` that borrows the underlying `Results`.
pub type ResultsRefCursor<'a, 'mir, 'tcx, A> = ResultsCursor<'mir, 'tcx, A, &'a Results<'tcx, A>>;

/// Allows random access inspection of the results of a dataflow analysis.
///
/// This cursor only has linear performance within a basic block when its statements are visited in
/// the same order as the `DIRECTION` of the analysis. In the worst case—when statements are
/// visited in *reverse* order—performance will be quadratic in the number of statements in the
/// block. The order in which basic blocks are inspected has no impact on performance.
///
/// A `ResultsCursor` can either own (the default) or borrow the dataflow results it inspects. The
/// type of ownership is determined by `R` (see `ResultsRefCursor` above).
pub struct ResultsCursor<'mir, 'tcx, A, R = Results<'tcx, A>>
where
    A: Analysis<'tcx>,
{
    body: &'mir mir::Body<'tcx>,
    results: R,
    state: BitSet<A::Idx>,

    pos: CursorPosition,

    /// Indicates that `state` has been modified with a custom effect.
    ///
    /// When this flag is set, we need to reset to an entry set before doing a seek.
    state_needs_reset: bool,
}

impl<'mir, 'tcx, A, R> ResultsCursor<'mir, 'tcx, A, R>
where
    A: Analysis<'tcx>,
    R: Borrow<Results<'tcx, A>>,
{
    /// Returns a new cursor that can inspect `results`.
    pub fn new(body: &'mir mir::Body<'tcx>, results: R) -> Self {
        let bits_per_block = results.borrow().entry_set_for_block(mir::START_BLOCK).domain_size();

        ResultsCursor {
            body,
            results,

            // Initialize to an empty `BitSet` and set `state_needs_reset` to tell the cursor that
            // it needs to reset to block entry before the first seek. The cursor position is
            // immaterial.
            state_needs_reset: true,
            state: BitSet::new_empty(bits_per_block),
            pos: CursorPosition::block_entry(mir::START_BLOCK),
        }
    }

    pub fn body(&self) -> &'mir mir::Body<'tcx> {
        self.body
    }

    /// Returns the `Analysis` used to generate the underlying results.
    pub fn analysis(&self) -> &A {
        &self.results.borrow().analysis
    }

    /// Returns the dataflow state at the current location.
    pub fn get(&self) -> &BitSet<A::Idx> {
        &self.state
    }

    /// Returns `true` if the dataflow state at the current location contains the given element.
    ///
    /// Shorthand for `self.get().contains(elem)`
    pub fn contains(&self, elem: A::Idx) -> bool {
        self.state.contains(elem)
    }

    /// Resets the cursor to hold the dataflow state for the given basic block at fixpoint.
    ///
    /// For forward dataflow analyses, this is the dataflow state prior to the first statement.
    ///
    /// For backward dataflow analyses, this is the dataflow state after the terminator.
    pub(super) fn seek_to_block_entry(&mut self, block: BasicBlock) {
        self.state.overwrite(&self.results.borrow().entry_set_for_block(block));
        self.pos = CursorPosition::block_entry(block);
        self.state_needs_reset = false;
    }

    /// Resets the cursor to hold the state at the entry to the given block.
    ///
    /// For forward analyses, this is the block's state at fixpoint.
    ///
    /// For backward analyses, this is the state that will be propagated to its
    /// predecessors (ignoring edge-specific effects).
    pub fn seek_to_block_start(&mut self, block: BasicBlock) {
        if A::Direction::is_forward() {
            self.seek_to_block_entry(block)
        } else {
            self.seek_after(Location { block, statement_index: 0 }, Effect::Primary)
        }
    }

    /// Resets the cursor to hold the state at the exit of the given block.
    ///
    /// For backward analyses, this is the block's state at fixpoint.
    ///
    /// For forward analyses, this is the state that will be propagated to its
    /// successors (ignoring edge-specific effects).
    pub fn seek_to_block_end(&mut self, block: BasicBlock) {
        if A::Direction::is_backward() {
            self.seek_to_block_entry(block)
        } else {
            self.seek_after(self.body.terminator_loc(block), Effect::Primary)
        }
    }

    /// Advances the cursor to hold the dataflow state at `target` before its "primary" effect is
    /// applied.
    ///
    /// The "before" effect at the target location *will be* applied.
    pub fn seek_before_primary_effect(&mut self, target: Location) {
        self.seek_after(target, Effect::Before)
    }

    /// Advances the cursor to hold the dataflow state at `target` after its "primary" effect is
    /// applied.
    ///
    /// The "before" effect at the target location will be applied as well.
    pub fn seek_after_primary_effect(&mut self, target: Location) {
        self.seek_after(target, Effect::Primary)
    }

    fn seek_after(&mut self, target: Location, effect: Effect) {
        assert!(target <= self.body.terminator_loc(target.block));

        // Reset to the entry of the target block if any of the following are true:
        //   - A custom effect has been applied to the cursor state.
        //   - We are in a different block than the target.
        //   - We are in the same block but have advanced past the target effect.
        if self.state_needs_reset || self.pos.block != target.block {
            self.seek_to_block_entry(target.block);
        } else if let Some(curr_effect) = self.pos.curr_effect_index {
            let mut ord = curr_effect.statement_index.cmp(&target.statement_index);
            if A::Direction::is_backward() {
                ord = ord.reverse()
            }

            match ord.then_with(|| curr_effect.effect.cmp(&effect)) {
                Ordering::Equal => return,
                Ordering::Greater => self.seek_to_block_entry(target.block),
                Ordering::Less => {}
            }
        }

        // At this point, the cursor is in the same block as the target location at an earlier
        // statement.
        debug_assert_eq!(target.block, self.pos.block);

        let block_data = &self.body[target.block];
        let next_effect = if A::Direction::is_forward() {
            #[rustfmt::skip]
            self.pos.curr_effect_index.map_or_else(
                || Effect::Before.at_index(0),
                EffectIndex::next_in_forward_order,
            )
        } else {
            self.pos.curr_effect_index.map_or_else(
                || Effect::Before.at_index(block_data.statements.len()),
                EffectIndex::next_in_backward_order,
            )
        };

        let analysis = &self.results.borrow().analysis;
        let target_effect_index = effect.at_index(target.statement_index);

        A::Direction::apply_effects_in_range(
            analysis,
            &mut self.state,
            target.block,
            block_data,
            next_effect..=target_effect_index,
        );

        self.pos =
            CursorPosition { block: target.block, curr_effect_index: Some(target_effect_index) };
    }

    /// Applies `f` to the cursor's internal state.
    ///
    /// This can be used, e.g., to apply the call return effect directly to the cursor without
    /// creating an extra copy of the dataflow state.
    pub fn apply_custom_effect(&mut self, f: impl FnOnce(&A, &mut BitSet<A::Idx>)) {
        f(&self.results.borrow().analysis, &mut self.state);
        self.state_needs_reset = true;
    }
}

#[derive(Clone, Copy, Debug)]
struct CursorPosition {
    block: BasicBlock,
    curr_effect_index: Option<EffectIndex>,
}

impl CursorPosition {
    fn block_entry(block: BasicBlock) -> CursorPosition {
        CursorPosition { block, curr_effect_index: None }
    }
}
