/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
mod layout;
mod stylo_taffy;
use std::fmt;

use app_units::Au;
use malloc_size_of_derive::MallocSizeOf;
use script::layout_dom::ServoLayoutNode;
use servo_arc::Arc;
use style::Atom;
use style::context::SharedStyleContext;
use style::properties::ComputedValues;
pub(crate) use stylo_taffy::TaffyStyloStyle;
use taffy::GridItemStyle;

use crate::PropagatedBoxTreeData;
use crate::cell::ArcRefCell;
use crate::construct_modern::{ModernContainerBuilder, ModernItemKind};
use crate::context::LayoutContext;
use crate::dom::{LayoutBox, WeakLayoutBox};
use crate::dom_traversal::{NodeAndStyleInfo, NonReplacedContents};
use crate::formatting_contexts::IndependentFormattingContext;
use crate::fragment_tree::{BoxFragment, FragmentChildren, FragmentFlags};
use crate::geom::{PhysicalPoint, PhysicalRect, PhysicalSize};
use crate::layout_box_base::LayoutBoxBase;
use crate::positioned::{AbsolutelyPositionedBox, PositioningContext};

/// Tracks and gap adopted from a parent grid for a subgridded axis
/// (phase-1 subgrid: definite, plain-length parent tracks only).
#[derive(Clone, Debug)]
pub(crate) struct SubgridAdoption {
    pub tracks: Vec<style::values::generics::grid::TrackListValue<
        style::values::computed::LengthPercentage,
        i32,
    >>,
    /// The parent grid's gap in the subgridded axis, in pixels.
    pub gap: f32,
}

impl malloc_size_of::MallocSizeOf for SubgridAdoption {
    fn size_of(&self, _ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        // The adopted track list is tiny (a handful of plain-length clones).
        self.tracks.len() * std::mem::size_of::<usize>()
    }
}

#[derive(Debug, MallocSizeOf)]
pub(crate) struct TaffyContainer {
    children: Vec<ArcRefCell<TaffyItemBox>>,
    style: Arc<ComputedValues>,
    fragment_flags: FragmentFlags,
    /// Adopted parent tracks when this grid is a subgrid in the column axis.
    pub(crate) subgrid_columns: Option<SubgridAdoption>,
    /// Adopted parent tracks when this grid is a subgrid in the row axis.
    pub(crate) subgrid_rows: Option<SubgridAdoption>,
}

impl TaffyContainer {
    pub fn construct(
        context: &LayoutContext,
        info: &NodeAndStyleInfo,
        contents: NonReplacedContents,
        propagated_data: PropagatedBoxTreeData,
        fragment_flags: FragmentFlags,
    ) -> Self {
        let mut builder = ModernContainerBuilder::new(context, info, propagated_data);
        contents.traverse(context, info, &mut builder);
        let items = builder.finish();

        let children: Vec<_> = items
            .into_iter()
            .map(|item| {
                let taffy_item_box = match item.kind {
                    ModernItemKind::InFlow(independent_formatting_context) => {
                        ArcRefCell::new(TaffyItemBox::new(TaffyItemBoxInner::InFlowBox(
                            independent_formatting_context,
                        )))
                    },
                    ModernItemKind::OutOfFlow(independent_formatting_context) => {
                        let abs_pos_box = ArcRefCell::new(AbsolutelyPositionedBox::new(
                            independent_formatting_context,
                        ));
                        ArcRefCell::new(TaffyItemBox::new(
                            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(abs_pos_box),
                        ))
                    },
                    ModernItemKind::ReusedBox(layout_box) => match layout_box {
                        LayoutBox::TaffyItemBox(taffy_item_box) => taffy_item_box,
                        _ => unreachable!("Undamaged taffy level element should be associated with taffy level box"),
                    },
                };

                item.box_slot.set(LayoutBox::TaffyItemBox(taffy_item_box.clone()));
                taffy_item_box
            })
            .collect();

        adopt_subgrid_tracks(&children, &info.style);

        Self {
            children,
            style: info.style.clone(),
            fragment_flags,
            subgrid_columns: None,
            subgrid_rows: None,
        }
    }

    pub(crate) fn repair_style(&mut self, new_style: &Arc<ComputedValues>) {
        self.style = new_style.clone();
    }
}

#[derive(MallocSizeOf)]
pub(crate) struct TaffyItemBox {
    pub(crate) taffy_layout: taffy::Layout,
    pub(crate) taffy_baselines: taffy::Baselines,
    pub(crate) child_fragments: FragmentChildren,
    pub(crate) positioning_context: PositioningContext,
    pub(crate) style: Arc<ComputedValues>,
    pub(crate) taffy_level_box: TaffyItemBoxInner,
}

#[expect(clippy::large_enum_variant)]
#[derive(Debug, MallocSizeOf)]
pub(crate) enum TaffyItemBoxInner {
    InFlowBox(IndependentFormattingContext),
    OutOfFlowAbsolutelyPositionedBox(ArcRefCell<AbsolutelyPositionedBox>),
}

impl fmt::Debug for TaffyItemBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaffyItemBox")
            .field("taffy_layout", &self.taffy_layout)
            .field("taffy_baselines", &self.taffy_baselines)
            .field("child_fragments", &self.child_fragments.len())
            .field("style", &self.style)
            .field("taffy_level_box", &self.taffy_level_box)
            .finish()
    }
}

impl TaffyItemBox {
    fn new(inner: TaffyItemBoxInner) -> Self {
        let style: Arc<ComputedValues> = match &inner {
            TaffyItemBoxInner::InFlowBox(item) => item.style().clone(),
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(absbox) => {
                (*absbox).borrow().context.style().clone()
            },
        };

        Self {
            taffy_layout: Default::default(),
            taffy_baselines: taffy::Baselines::NONE,
            child_fragments: Default::default(),
            positioning_context: PositioningContext::default(),
            style,
            taffy_level_box: inner,
        }
    }

    pub(crate) fn with_base<T>(&self, callback: impl FnOnce(&LayoutBoxBase) -> T) -> T {
        match self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(ref independent_formatting_context) => {
                callback(&independent_formatting_context.base)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(ref positioned_box) => {
                callback(&positioned_box.borrow().context.base)
            },
        }
    }

    pub(crate) fn with_base_mut<T>(&mut self, callback: impl FnOnce(&mut LayoutBoxBase) -> T) -> T {
        match &mut self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(independent_formatting_context) => {
                callback(&mut independent_formatting_context.base)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(positioned_box) => {
                callback(&mut positioned_box.borrow_mut().context.base)
            },
        }
    }

    pub(crate) fn repair_style(
        &mut self,
        context: &SharedStyleContext,
        node: &ServoLayoutNode,
        new_style: &Arc<ComputedValues>,
    ) {
        self.style = new_style.clone();
        match &mut self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(independent_formatting_context) => {
                independent_formatting_context.repair_style(context, node, new_style)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(positioned_box) => positioned_box
                .borrow_mut()
                .context
                .repair_style(context, node, new_style),
        }
    }

    pub(crate) fn attached_to_tree(&self, layout_box: WeakLayoutBox) {
        match &self.taffy_level_box {
            TaffyItemBoxInner::InFlowBox(formatting_context) => {
                formatting_context.attached_to_tree(layout_box)
            },
            TaffyItemBoxInner::OutOfFlowAbsolutelyPositionedBox(positioned_box) => positioned_box
                .borrow_mut()
                .context
                .attached_to_tree(layout_box),
        }
    }
}

/// Details from Taffy grid layout that will be stored
#[derive(Clone, Debug, MallocSizeOf)]
pub(crate) struct SpecificTaffyGridInfo {
    pub info: taffy::DetailedGridInfo<Atom>,
}

impl SpecificTaffyGridInfo {
    fn from_detailed_grid_layout(grid_info: taffy::DetailedGridInfo<Atom>) -> Self {
        Self { info: grid_info }
    }

    /// Resolve the grid area of an absolutely positioned grid item, returning a rectangle
    /// relative to the grid container's content box.
    pub(crate) fn resolve_grid_area(
        &self,
        containing_block_fragment: &BoxFragment,
        item_style: &ComputedValues,
    ) -> PhysicalRect<Au> {
        // Fragment flags are not used when resolving the grid area, so empty flags are
        // sufficient here.
        let item_style = TaffyStyloStyle::new(item_style, FragmentFlags::empty());

        // Convert direction to Taffy type
        let direction = if containing_block_fragment.style().writing_mode.is_bidi_ltr() {
            taffy::Direction::Ltr
        } else {
            taffy::Direction::Rtl
        };

        // Convert padding box to Taffy type
        let containing_block_border = containing_block_fragment.border;
        let border_left = containing_block_border.left.to_f32_px();
        let border_top = containing_block_border.top.to_f32_px();

        let containing_block_padding_rect = containing_block_fragment.padding_rect();
        let padding_box = taffy::Rect {
            left: border_left,
            right: border_left + containing_block_padding_rect.width().to_f32_px(),
            top: border_top,
            bottom: border_top + containing_block_padding_rect.height().to_f32_px(),
        };

        // Call into Taffy to resolve grid area
        let area = self.info.resolve_absolute_grid_area(
            item_style.grid_row(),
            item_style.grid_column(),
            direction,
            padding_box,
        );

        // Convert grid area into a PhysicalRect, and adjust it to be relative to the content box.
        let containing_block_padding = containing_block_fragment.padding;
        PhysicalRect::new(
            PhysicalPoint::new(
                Au::from_f32_px(area.left) -
                    (containing_block_border.left + containing_block_padding.left),
                Au::from_f32_px(area.top) -
                    (containing_block_border.top + containing_block_padding.top),
            ),
            PhysicalSize::new(
                Au::from_f32_px(area.right - area.left),
                Au::from_f32_px(area.bottom - area.top),
            ),
        )
    }
}


/// Phase-1 subgrid support: for each grid item that is itself a grid container
/// with `subgrid` in an axis, adopt the parent's spanned tracks when they are
/// all definite plain lengths. Anything more complex (fr, auto, percentages,
/// content sizing, auto placement) falls back to laying out as a normal grid.
fn adopt_subgrid_tracks(children: &[ArcRefCell<TaffyItemBox>], container_style: &ComputedValues) {
    use style::values::generics::grid::GenericGridTemplateComponent;

    let position = container_style.get_position();
    for child in children {
        let mut child = (**child).borrow_mut();
        let child_style = child.style.clone();
        let child_position = child_style.get_position();

        let column_adoption = match &child_position.grid_template_columns {
            GenericGridTemplateComponent::Subgrid(_) => compute_subgrid_adoption(
                &position.grid_template_columns,
                &child_position.grid_column_start,
                &child_position.grid_column_end,
                &position.column_gap,
            ),
            _ => None,
        };
        let row_adoption = match &child_position.grid_template_rows {
            GenericGridTemplateComponent::Subgrid(_) => compute_subgrid_adoption(
                &position.grid_template_rows,
                &child_position.grid_row_start,
                &child_position.grid_row_end,
                &position.row_gap,
            ),
            _ => None,
        };

        if column_adoption.is_none() && row_adoption.is_none() {
            continue;
        }
        if let TaffyItemBoxInner::InFlowBox(independent_context) = &mut child.taffy_level_box {
            if let Some(grid) = independent_context.grid_container_mut() {
                grid.subgrid_columns = column_adoption;
                grid.subgrid_rows = row_adoption;
            }
        }
    }
}

/// Compute the adopted track list for one subgridded axis, or `None` if the
/// parent tracks or the child's placement are outside the phase-1 subset.
fn compute_subgrid_adoption(
    parent_template: &style::values::computed::GridTemplateComponent,
    placement_start: &style::values::computed::GridLine,
    placement_end: &style::values::computed::GridLine,
    parent_gap: &style::values::generics::length::LengthPercentageOrNormal<
        style::values::generics::NonNegative<style::values::computed::LengthPercentage>,
    >,
) -> Option<SubgridAdoption> {
    use style::values::computed::length_percentage::Unpacked as UnpackedLengthPercentage;
    use style::values::generics::grid::{
        GenericGridTemplateComponent, GenericTrackBreadth, GenericTrackListValue, GenericTrackSize,
        RepeatCount,
    };

    // The parent must have an explicit track list in this axis.
    let track_values = match parent_template {
        GenericGridTemplateComponent::TrackList(list) => &list.values,
        _ => return None,
    };

    // Expand simple fixed-count repeats into a flat track list.
    let mut expanded: Vec<GenericTrackListValue<style::values::computed::LengthPercentage, i32>> =
        Vec::new();
    for value in track_values.iter() {
        match value {
            GenericTrackListValue::TrackSize(size) => expanded.push(value.clone()),
            GenericTrackListValue::TrackRepeat(repeat) => match repeat.count {
                RepeatCount::Number(count) => {
                    for _ in 0..count {
                        for size in repeat.track_sizes.iter() {
                            expanded
                                .push(GenericTrackListValue::TrackSize(size.clone()));
                        }
                    }
                },
                _ => return None,
            },
        }
    }

    // Every expanded track must be a definite plain length (no percentages,
    // calc, fr, minmax, fit-content or content sizing).
    for value in &expanded {
        if let GenericTrackListValue::TrackSize(GenericTrackSize::Breadth(
            GenericTrackBreadth::Breadth(length_percentage),
        )) = value
        {
            if !matches!(
                length_percentage.unpack(),
                UnpackedLengthPercentage::Length(_)
            ) {
                return None;
            }
        } else {
            return None;
        }
    }
    let track_count = expanded.len() as i32;

    // Resolve the child's placement in this axis into a track index range.
    if placement_start.is_span || placement_start.line_num <= 0 {
        return None;
    }
    let start_line = placement_start.line_num;
    let end_line = if placement_end.is_span {
        if placement_end.line_num <= 0 {
            return None;
        }
        start_line + placement_end.line_num
    } else if placement_end.line_num > 0 {
        placement_end.line_num
    } else if placement_end.line_num < 0 {
        // Negative lines count from the end: -1 is the last line.
        (track_count + 2) + placement_end.line_num
    } else {
        return None;
    };
    let first_track = start_line - 1;
    let last_track = end_line - 2; // exclusive
    if first_track < 0 || last_track < first_track || last_track >= track_count {
        return None;
    }
    let tracks: Vec<_> = expanded[(first_track as usize)..=(last_track as usize)].to_vec();
    if tracks.is_empty() {
        return None;
    }

    // Resolve the parent's gap in this axis.
    let gap = match parent_gap {
        style::values::generics::length::LengthPercentageOrNormal::Normal => 0.0,
        style::values::generics::length::LengthPercentageOrNormal::LengthPercentage(value) => {
            match value.0.unpack() {
                UnpackedLengthPercentage::Length(length) => length.px(),
                _ => return None,
            }
        },
    };

    Some(SubgridAdoption { tracks, gap })
}
