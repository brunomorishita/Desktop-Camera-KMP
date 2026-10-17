// Decoding process state of ITU-T H.264 section 8.2: picture order count, reference
// picture marking, reference list construction and output order. Independent of the
// hardware API: `H` is whatever the backend uses to name a decoded surface
use super::{
    params::Sps,
    slice::{ListModification, MemoryOp, SliceHeader, SliceType},
    ParseError,
};

// Frames the output may be held back for reordering at most (the DPB size limit)
const MAX_REORDER: usize = 16;

#[derive(Debug, Clone)]
pub struct RefPicture<H> {
    // None for the frames inferred from a frame_num gap: they hold no picture
    pub handle: Option<H>,
    pub frame_num: u32,
    // LongTermFrameIdx when marked as long-term reference
    pub long_term: Option<u32>,
    pub top_poc: i32,
    pub bottom_poc: i32,
    id: u64,
}

impl<H> RefPicture<H> {
    pub fn is_long_term(&self) -> bool {
        self.long_term.is_some()
    }

    pub fn poc(&self) -> i32 {
        self.top_poc.min(self.bottom_poc)
    }
}

// The picture being decoded, from its first slice
#[derive(Debug, Clone)]
pub struct CurrentPicture {
    pub frame_num: u32,
    pub top_poc: i32,
    pub bottom_poc: i32,
    pub is_reference: bool,
    pub idr: bool,
    // PicOrderCntMsb and pic_order_cnt_lsb (POC type 0), the next picture's prediction
    poc_msb: i32,
    poc_lsb: i32,
    long_term_reference: bool,
    memory_ops: Option<Vec<MemoryOp>>,
}

impl CurrentPicture {
    pub fn poc(&self) -> i32 {
        self.top_poc.min(self.bottom_poc)
    }

    fn has_mmco5(&self) -> bool {
        self.memory_ops
            .as_ref()
            .is_some_and(|ops| ops.contains(&MemoryOp::UnmarkAll))
    }
}

pub struct Dpb<H> {
    refs: Vec<RefPicture<H>>,
    // Decoded, not yet output: (POC, picture)
    pending: Vec<(i32, H)>,
    max_long_term_idx: Option<u32>,
    next_id: u64,
    // Previous picture state needed by 8.2.1 and 8.2.5.2
    prev_ref_frame_num: u32,
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num: u32,
    prev_frame_num_offset: i32,
    prev_had_mmco5: bool,
    // Output reordering depth, see output()
    reorder_depth: usize,
    adaptive_reorder: bool,
    last_output_poc: Option<i32>,
}

impl<H: Clone> Default for Dpb<H> {
    fn default() -> Self {
        Dpb {
            refs: Vec::new(),
            pending: Vec::new(),
            max_long_term_idx: None,
            next_id: 0,
            prev_ref_frame_num: 0,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            prev_frame_num: 0,
            prev_frame_num_offset: 0,
            prev_had_mmco5: false,
            reorder_depth: 0,
            adaptive_reorder: false,
            last_output_poc: None,
        }
    }
}

impl<H: Clone> Dpb<H> {
    // Pictures that may be referenced by the current one (VA ReferenceFrames)
    pub fn references(&self) -> impl Iterator<Item = &RefPicture<H>> {
        self.refs.iter().filter(|picture| picture.handle.is_some())
    }

    // Everything decoded so far, in output order (end of stream or a new sequence)
    pub fn flush(&mut self) -> Vec<H> {
        let mut pending = std::mem::take(&mut self.pending);
        pending.sort_by_key(|(poc, _)| *poc);
        self.last_output_poc = None;
        pending.into_iter().map(|(_, handle)| handle).collect()
    }

    // Starts a picture: computes its POC and fills frame_num gaps. Returns the pictures
    // to output before it (all of them on an IDR)
    pub fn begin_picture(
        &mut self,
        header: &SliceHeader,
        sps: &Sps,
    ) -> Result<(CurrentPicture, Vec<H>), ParseError> {
        let mut output = Vec::new();
        if header.idr {
            output = self.flush();
            self.refs.clear();
            self.max_long_term_idx = None;
            self.prev_ref_frame_num = 0;
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
            self.prev_frame_num = 0;
            self.prev_frame_num_offset = 0;
            self.prev_had_mmco5 = false;
            self.configure_reordering(sps);
        } else {
            self.fill_frame_num_gap(header.frame_num, sps);
        }

        let (top_poc, bottom_poc, poc_msb) = self.picture_order_count(header, sps)?;
        Ok((
            CurrentPicture {
                frame_num: header.frame_num,
                top_poc,
                bottom_poc,
                is_reference: header.nal_ref_idc != 0,
                idr: header.idr,
                poc_msb,
                poc_lsb: header.pic_order_cnt_lsb as i32,
                long_term_reference: header.long_term_reference,
                memory_ops: header.memory_ops.clone(),
            },
            output,
        ))
    }

    // RefPicList0/1 of a slice (8.2.4), `num_ref_idx_active` entries each
    pub fn reference_lists(
        &self,
        header: &SliceHeader,
        current: &CurrentPicture,
        sps: &Sps,
    ) -> Result<[Vec<Option<RefPicture<H>>>; 2], ParseError> {
        let mut lists = [Vec::new(), Vec::new()];
        let count = header.slice_type.list_count();
        if count == 0 {
            return Ok(lists);
        }

        let max_frame_num = sps.max_frame_num() as i32;
        let pic_num = |picture: &RefPicture<H>| {
            let frame_num = picture.frame_num as i32;
            if frame_num > current.frame_num as i32 {
                frame_num - max_frame_num
            } else {
                frame_num
            }
        };
        let mut short_term: Vec<&RefPicture<H>> =
            self.refs.iter().filter(|p| !p.is_long_term()).collect();
        let mut long_term: Vec<&RefPicture<H>> =
            self.refs.iter().filter(|p| p.is_long_term()).collect();
        long_term.sort_by_key(|p| p.long_term);

        let initial: [Vec<&RefPicture<H>>; 2] = if header.slice_type == SliceType::B {
            let poc = current.poc();
            short_term.sort_by_key(|p| p.poc());
            let before: Vec<_> = short_term
                .iter()
                .rev()
                .filter(|p| p.poc() < poc)
                .copied()
                .collect();
            let after: Vec<_> = short_term
                .iter()
                .filter(|p| p.poc() > poc)
                .copied()
                .collect();
            let list0: Vec<_> = before
                .iter()
                .chain(&after)
                .chain(&long_term)
                .copied()
                .collect();
            let mut list1: Vec<_> = after
                .iter()
                .chain(&before)
                .chain(&long_term)
                .copied()
                .collect();
            if list1.len() > 1 && list0.iter().map(|p| p.id).eq(list1.iter().map(|p| p.id)) {
                list1.swap(0, 1);
            }
            [list0, list1]
        } else {
            short_term.sort_by_key(|p| std::cmp::Reverse(pic_num(p)));
            [
                short_term.into_iter().chain(long_term).collect(),
                Vec::new(),
            ]
        };

        for (list, initial) in initial.into_iter().enumerate().take(count) {
            let active = header.num_ref_idx_active[list] as usize;
            let mut entries: Vec<Option<&RefPicture<H>>> = initial.into_iter().map(Some).collect();
            entries.resize(active.max(entries.len()), None);
            entries.truncate(active);

            // 8.2.4.3
            let mut pic_num_pred = current.frame_num as i32;
            for (ref_idx, modification) in header.list_modifications[list].iter().enumerate() {
                let picture = match *modification {
                    ListModification::ShortTerm(delta) => {
                        let mut no_wrap = pic_num_pred + delta;
                        if no_wrap < 0 {
                            no_wrap += max_frame_num;
                        } else if no_wrap >= max_frame_num {
                            no_wrap -= max_frame_num;
                        }
                        pic_num_pred = no_wrap;
                        let target = if no_wrap > current.frame_num as i32 {
                            no_wrap - max_frame_num
                        } else {
                            no_wrap
                        };
                        self.refs
                            .iter()
                            .find(|p| !p.is_long_term() && pic_num(p) == target)
                    }
                    ListModification::LongTerm(long_term_pic_num) => self
                        .refs
                        .iter()
                        .find(|p| p.long_term == Some(long_term_pic_num)),
                };
                let picture = picture.ok_or(ParseError::Invalid(
                    "list modification names a missing reference",
                ))?;
                if ref_idx >= active {
                    return Err(ParseError::Invalid(
                        "more list modifications than references",
                    ));
                }

                entries.insert(ref_idx, Some(picture));
                let mut kept = ref_idx + 1;
                for index in ref_idx + 1..entries.len() {
                    if entries[index].map(|p| p.id) != Some(picture.id) {
                        entries[kept] = entries[index];
                        kept += 1;
                    }
                }
                entries.truncate(kept.min(active));
                entries.resize(active, None);
            }

            lists[list] = entries.into_iter().map(|entry| entry.cloned()).collect();
        }
        Ok(lists)
    }

    // Marks references after decoding (8.2.5) and returns the pictures now due for output
    pub fn finish_picture(&mut self, current: CurrentPicture, handle: H, sps: &Sps) -> Vec<H> {
        let mut output = Vec::new();
        let mut current = current;
        let mmco5 = current.has_mmco5();

        if current.is_reference {
            let mut long_term = None;
            if current.idr {
                if current.long_term_reference {
                    long_term = Some(0);
                    self.max_long_term_idx = Some(0);
                }
            } else if let Some(ops) = current.memory_ops.clone() {
                for op in ops {
                    self.apply_memory_op(op, &current, &mut long_term, sps);
                }
            } else {
                self.sliding_window(current.frame_num, sps);
            }

            if mmco5 {
                // 8.2.1: the picture restarts the POC and frame_num counting
                output = self.flush();
                let poc = current.poc();
                current.top_poc -= poc;
                current.bottom_poc -= poc;
                current.frame_num = 0;
            }

            self.next_id += 1;
            self.refs.push(RefPicture {
                handle: Some(handle.clone()),
                frame_num: current.frame_num,
                long_term,
                top_poc: current.top_poc,
                bottom_poc: current.bottom_poc,
                id: self.next_id,
            });
            // Streams with more references than declared: keep the newest ones
            let max_refs = sps.max_num_ref_frames.max(1) as usize;
            while self.refs.len() > max_refs {
                let oldest = self
                    .refs
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| !p.is_long_term())
                    .min_by_key(|(_, p)| self.frame_num_wrap(p.frame_num, current.frame_num, sps))
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                self.refs.remove(oldest);
            }

            self.prev_ref_frame_num = current.frame_num;
            // After an mmco 5 the prediction restarts from the picture's reset POC
            (self.prev_poc_msb, self.prev_poc_lsb) = if mmco5 {
                (0, current.top_poc)
            } else {
                (current.poc_msb, current.poc_lsb)
            };
        }

        self.prev_frame_num = current.frame_num;
        self.prev_had_mmco5 = mmco5;

        output.extend(self.output(current.poc(), handle));
        output
    }

    fn configure_reordering(&mut self, sps: &Sps) {
        // Without a declared depth, streams that cannot reorder (POC type 2, Baseline)
        // are shown at once; others start at 0 and grow when a picture comes late
        let (depth, adaptive) = match sps.max_num_reorder_frames {
            Some(depth) => (depth as usize, false),
            None if sps.pic_order_cnt_type == 2 || sps.profile_idc == 66 => (0, false),
            None => (0, true),
        };
        self.reorder_depth = depth.min(MAX_REORDER);
        self.adaptive_reorder = adaptive;
    }

    // C.4.5.3 "bumping", with a fixed depth instead of the DPB fullness: low latency
    fn output(&mut self, poc: i32, handle: H) -> Vec<H> {
        if self.adaptive_reorder
            && self.last_output_poc.is_some_and(|last| poc < last)
            && self.reorder_depth < MAX_REORDER
        {
            self.reorder_depth += 1;
        }

        self.pending.push((poc, handle));
        let mut output = Vec::new();
        while self.pending.len() > self.reorder_depth {
            let (index, _) = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, (poc, _))| *poc)
                .expect("pending is not empty");
            let (poc, handle) = self.pending.remove(index);
            self.last_output_poc = Some(poc);
            output.push(handle);
        }
        output
    }

    fn frame_num_wrap(&self, frame_num: u32, current_frame_num: u32, sps: &Sps) -> i32 {
        if frame_num > current_frame_num {
            frame_num as i32 - sps.max_frame_num() as i32
        } else {
            frame_num as i32
        }
    }

    // 8.2.5.3
    fn sliding_window(&mut self, current_frame_num: u32, sps: &Sps) {
        let max_refs = sps.max_num_ref_frames.max(1) as usize;
        while self.refs.len() >= max_refs {
            let Some((index, _)) = self
                .refs
                .iter()
                .enumerate()
                .filter(|(_, p)| !p.is_long_term())
                .min_by_key(|(_, p)| self.frame_num_wrap(p.frame_num, current_frame_num, sps))
            else {
                break;
            };
            self.refs.remove(index);
        }
    }

    // 8.2.5.4
    fn apply_memory_op(
        &mut self,
        op: MemoryOp,
        current: &CurrentPicture,
        long_term: &mut Option<u32>,
        sps: &Sps,
    ) {
        let short_term_pic_num = |dpb: &Self, difference: u32| {
            let target = current.frame_num as i32 - difference as i32;
            dpb.refs.iter().position(|p| {
                !p.is_long_term()
                    && dpb.frame_num_wrap(p.frame_num, current.frame_num, sps) == target
            })
        };
        match op {
            MemoryOp::UnmarkShortTerm(difference) => {
                if let Some(index) = short_term_pic_num(self, difference) {
                    self.refs.remove(index);
                }
            }
            MemoryOp::UnmarkLongTerm(long_term_pic_num) => {
                self.refs.retain(|p| p.long_term != Some(long_term_pic_num));
            }
            MemoryOp::ShortTermToLongTerm(difference, idx) => {
                if let Some(index) = short_term_pic_num(self, difference) {
                    let id = self.refs[index].id;
                    self.refs.retain(|p| p.long_term != Some(idx) || p.id == id);
                    if let Some(picture) = self.refs.iter_mut().find(|p| p.id == id) {
                        picture.long_term = Some(idx);
                    }
                }
            }
            MemoryOp::SetMaxLongTermIdx(max_plus1) => {
                self.max_long_term_idx = max_plus1.checked_sub(1);
                let max = self.max_long_term_idx;
                self.refs.retain(|p| {
                    p.long_term
                        .is_none_or(|idx| max.is_some_and(|max| idx <= max))
                });
            }
            MemoryOp::UnmarkAll => {
                self.refs.clear();
                self.max_long_term_idx = None;
            }
            MemoryOp::CurrentToLongTerm(idx) => {
                self.refs.retain(|p| p.long_term != Some(idx));
                *long_term = Some(idx);
            }
        }
    }

    // 8.2.5.2: frames missing from the stream are inferred as "non-existing" references,
    // so the sliding window and the PicNums stay as the encoder had them
    fn fill_frame_num_gap(&mut self, frame_num: u32, sps: &Sps) {
        let max_frame_num = sps.max_frame_num();
        let expected = (self.prev_ref_frame_num + 1) % max_frame_num;
        if frame_num == self.prev_ref_frame_num || frame_num == expected {
            return;
        }

        let missing = (frame_num + max_frame_num - expected) % max_frame_num;
        // Older ones would be pushed out by the sliding window right away
        let kept = missing.min(sps.max_num_ref_frames.max(1));
        let mut unused = (frame_num + max_frame_num - kept) % max_frame_num;
        for _ in 0..kept {
            self.sliding_window(unused, sps);
            self.next_id += 1;
            self.refs.push(RefPicture {
                handle: None,
                frame_num: unused,
                long_term: None,
                top_poc: 0,
                bottom_poc: 0,
                id: self.next_id,
            });
            // FrameNumOffset still advances when the gap wraps frame_num (POC types 1, 2)
            if self.prev_frame_num > unused {
                let base = if self.prev_had_mmco5 {
                    0
                } else {
                    self.prev_frame_num_offset
                };
                self.prev_frame_num_offset = base + max_frame_num as i32;
                self.prev_had_mmco5 = false;
            }
            self.prev_frame_num = unused;
            unused = (unused + 1) % max_frame_num;
        }
        self.prev_ref_frame_num = (frame_num + max_frame_num - 1) % max_frame_num;
    }

    // 8.2.1, frames only: (TopFieldOrderCnt, BottomFieldOrderCnt, PicOrderCntMsb)
    fn picture_order_count(
        &mut self,
        header: &SliceHeader,
        sps: &Sps,
    ) -> Result<(i32, i32, i32), ParseError> {
        match sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1i32 << sps.log2_max_pic_order_cnt_lsb;
                let lsb = header.pic_order_cnt_lsb as i32;
                let (prev_msb, prev_lsb) = (self.prev_poc_msb, self.prev_poc_lsb);
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                Ok((top, top + header.delta_pic_order_cnt_bottom, msb))
            }
            1 => {
                let frame_num_offset = self.frame_num_offset(header, sps);
                let cycle = sps.offset_for_ref_frame.len() as i32;
                let mut abs_frame_num = if cycle != 0 {
                    frame_num_offset + header.frame_num as i32
                } else {
                    0
                };
                if header.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0;
                if abs_frame_num > 0 {
                    let delta_per_cycle: i32 = sps.offset_for_ref_frame.iter().sum();
                    let cycle_count = (abs_frame_num - 1) / cycle;
                    let in_cycle = ((abs_frame_num - 1) % cycle) as usize;
                    expected = cycle_count * delta_per_cycle
                        + sps.offset_for_ref_frame[..=in_cycle].iter().sum::<i32>();
                }
                if header.nal_ref_idc == 0 {
                    expected += sps.offset_for_non_ref_pic;
                }
                let top = expected + header.delta_pic_order_cnt[0];
                let bottom =
                    top + sps.offset_for_top_to_bottom_field + header.delta_pic_order_cnt[1];
                self.prev_frame_num_offset = frame_num_offset;
                Ok((top, bottom, 0))
            }
            _ => {
                let frame_num_offset = self.frame_num_offset(header, sps);
                let poc = if header.idr {
                    0
                } else if header.nal_ref_idc == 0 {
                    2 * (frame_num_offset + header.frame_num as i32) - 1
                } else {
                    2 * (frame_num_offset + header.frame_num as i32)
                };
                self.prev_frame_num_offset = frame_num_offset;
                Ok((poc, poc, 0))
            }
        }
    }

    fn frame_num_offset(&self, header: &SliceHeader, sps: &Sps) -> i32 {
        if header.idr {
            return 0;
        }
        let prev_offset = if self.prev_had_mmco5 {
            0
        } else {
            self.prev_frame_num_offset
        };
        if self.prev_frame_num > header.frame_num {
            prev_offset + sps.max_frame_num() as i32
        } else {
            prev_offset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264::testing::{idr, slice, sps, with_memory_ops};

    // Decodes a picture whose handle is its decode index, returns the pictures output
    fn decode(dpb: &mut Dpb<u32>, header: &SliceHeader, sps: &Sps, handle: u32) -> Vec<u32> {
        let (current, mut output) = dpb.begin_picture(header, sps).unwrap();
        output.extend(dpb.finish_picture(current, handle, sps));
        output
    }

    // Handles in a slice's list (None for missing or non-existing entries)
    fn list(dpb: &Dpb<u32>, header: &SliceHeader, sps: &Sps, index: usize) -> Vec<Option<u32>> {
        let (current, _) = Dpb::clone_state(dpb).begin_picture(header, sps).unwrap();
        dpb.reference_lists(header, &current, sps).unwrap()[index]
            .iter()
            .map(|entry| entry.as_ref().and_then(|p| p.handle))
            .collect()
    }

    impl Dpb<u32> {
        // begin_picture updates the POC state, lists are computed on a copy
        fn clone_state(&self) -> Dpb<u32> {
            Dpb {
                refs: self.refs.clone(),
                pending: self.pending.clone(),
                ..*self
            }
        }
    }

    #[test]
    fn p_list_follows_pic_num_across_the_frame_num_wrap() {
        // 16 frame numbers, 3 references
        let sps = sps(4, 2, 3);
        let mut dpb = Dpb::default();
        decode(&mut dpb, &idr(), &sps, 0);
        for frame_num in 1..16 {
            decode(
                &mut dpb,
                &slice(SliceType::P, frame_num, 0, true),
                &sps,
                frame_num,
            );
        }
        decode(&mut dpb, &slice(SliceType::P, 0, 0, true), &sps, 16);

        // Current frame_num 1: PicNums 0 (handle 16), -1 (15), -2 (14), oldest slid out
        let current = slice(SliceType::P, 1, 0, true);
        assert_eq!(
            list(&dpb, &current, &sps, 0),
            [Some(16), Some(15), Some(14), None]
        );
    }

    #[test]
    fn memory_operations_move_a_frame_to_long_term() {
        let sps = sps(4, 2, 4);
        let mut dpb = Dpb::default();
        decode(&mut dpb, &idr(), &sps, 0);
        decode(&mut dpb, &slice(SliceType::P, 1, 0, true), &sps, 1);
        decode(&mut dpb, &slice(SliceType::P, 2, 0, true), &sps, 2);
        // Frame 3 makes frame 1 (difference 2) long-term index 0 and drops frame 2 (difference 1)
        let marking = with_memory_ops(
            slice(SliceType::P, 3, 0, true),
            vec![
                MemoryOp::ShortTermToLongTerm(2, 0),
                MemoryOp::UnmarkShortTerm(1),
            ],
        );
        decode(&mut dpb, &marking, &sps, 3);

        // Short-term by PicNum (3, 0), then long-term
        let current = slice(SliceType::P, 4, 0, true);
        assert_eq!(
            list(&dpb, &current, &sps, 0),
            [Some(3), Some(0), Some(1), None]
        );

        // Modification: long-term 0 first, then PicNum 0
        let mut modified = current.clone();
        modified.list_modifications[0] = vec![
            ListModification::LongTerm(0),
            ListModification::ShortTerm(-4),
        ];
        assert_eq!(
            list(&dpb, &modified, &sps, 0),
            [Some(1), Some(0), Some(3), None]
        );
    }

    #[test]
    fn frame_num_gap_inserts_non_existing_references() {
        let sps = sps(4, 2, 4);
        let mut dpb = Dpb::default();
        decode(&mut dpb, &idr(), &sps, 0);
        decode(&mut dpb, &slice(SliceType::P, 1, 0, true), &sps, 1);
        // Frames 2 and 3 lost
        decode(&mut dpb, &slice(SliceType::P, 4, 0, true), &sps, 4);

        // PicNums 4, 3 (missing), 2 (missing), 1: the indices still match the encoder's
        let current = slice(SliceType::P, 5, 0, true);
        assert_eq!(
            list(&dpb, &current, &sps, 0),
            [Some(4), None, None, Some(1)]
        );
    }

    #[test]
    fn b_lists_order_by_poc_around_the_current_picture() {
        let sps = sps(4, 0, 4);
        let mut dpb = Dpb::default();
        // Decode order I0 P8 P4 (POC lsb = 2 * display index)
        decode(&mut dpb, &idr(), &sps, 0);
        decode(&mut dpb, &slice(SliceType::P, 1, 16, true), &sps, 1);
        decode(&mut dpb, &slice(SliceType::B, 2, 8, true), &sps, 2);

        // B at POC 4: before it 0; after it 8, 16
        let b = slice(SliceType::B, 3, 4, false);
        assert_eq!(list(&dpb, &b, &sps, 0), [Some(0), Some(2), Some(1), None]);
        assert_eq!(list(&dpb, &b, &sps, 1), [Some(2), Some(1), Some(0), None]);

        // Past every reference: both lists equal, so the first two of list 1 swap
        let late = slice(SliceType::B, 3, 20, false);
        assert_eq!(
            list(&dpb, &late, &sps, 0),
            [Some(1), Some(2), Some(0), None]
        );
        assert_eq!(
            list(&dpb, &late, &sps, 1),
            [Some(2), Some(1), Some(0), None]
        );
    }

    #[test]
    fn output_follows_poc_with_the_declared_reorder_depth() {
        let mut sps = sps(4, 0, 4);
        sps.max_num_reorder_frames = Some(1);
        let mut dpb = Dpb::default();
        let mut shown = decode(&mut dpb, &idr(), &sps, 0);
        // Decode order I0 P2 B1 P4 B3 (POC lsb 2 * display index)
        shown.extend(decode(&mut dpb, &slice(SliceType::P, 1, 4, true), &sps, 2));
        shown.extend(decode(&mut dpb, &slice(SliceType::B, 2, 2, false), &sps, 1));
        shown.extend(decode(&mut dpb, &slice(SliceType::P, 2, 8, true), &sps, 4));
        shown.extend(decode(&mut dpb, &slice(SliceType::B, 3, 6, false), &sps, 3));
        shown.extend(dpb.flush());
        assert_eq!(shown, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn undeclared_reordering_is_learned_from_the_first_late_picture() {
        let sps = sps(4, 0, 4);
        let mut dpb = Dpb::default();
        let mut shown = decode(&mut dpb, &idr(), &sps, 0);
        shown.extend(decode(&mut dpb, &slice(SliceType::P, 1, 4, true), &sps, 2));
        // Arrives after POC 4 was shown: shown late once, then the depth grows to 1
        shown.extend(decode(&mut dpb, &slice(SliceType::B, 2, 2, false), &sps, 1));
        shown.extend(decode(&mut dpb, &slice(SliceType::P, 2, 8, true), &sps, 4));
        shown.extend(decode(&mut dpb, &slice(SliceType::B, 3, 6, false), &sps, 3));
        shown.extend(dpb.flush());
        assert_eq!(shown, [0, 2, 1, 3, 4]);
    }

    #[test]
    fn poc_lsb_wrap_keeps_increasing() {
        // 6 bit LSB (64 values) counting up by 20: 20, 40, 60, then 2 is 66, not 2.
        // A single jump of more than half the range would be read as going backwards
        let sps = sps(4, 0, 2);
        let mut dpb = Dpb::default();
        decode(&mut dpb, &idr(), &sps, 0);
        for (frame_num, lsb) in [(1, 20), (2, 40), (3, 60)] {
            decode(&mut dpb, &slice(SliceType::P, frame_num, lsb, true), &sps, frame_num);
        }
        let (current, _) = dpb.begin_picture(&slice(SliceType::P, 4, 2, true), &sps).unwrap();
        assert_eq!(current.poc(), 66);
    }
}
