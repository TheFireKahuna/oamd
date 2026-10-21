//! Object Audio Metadata (OAMD) structures
//!
//! This module contains structures for handling Object Audio Metadata,
//! which provides spatial audio information for immersive audio playback.

use std::default::Default;

use crate::bitstream::SliceReader;
use anyhow::Result;
use log::{trace, warn};

pub const MAX_OBJECT_COUNT: usize = 159;
/// Upper bound of `num_obj_info_blocks`, which is coded as a 3-bit minus-1 field.
pub const MAX_OBJ_INFO_BLOCKS: usize = 8;
pub const GAIN_MINUS_INFINITY: i8 = -128; // -inf gain

#[derive(Clone, Debug)]
#[repr(C)]
struct OAMDParserState {
    object_count: usize,
    program_assignment: ProgramAssignment,
    b_alternate_object_data_present: bool,

    /// Predictor for `object_gain_code == 0b11`: the gain of the last object read, at each
    /// block index (i.e. the previous object's gain at that block).
    prev_object_gain: [i8; MAX_OBJ_INFO_BLOCKS],
    prev_object_basic_info: ObjectBasicInfo,
    prev_object_render_info: ObjectRenderInfo,

    object_element: Option<ObjectElement>,
    trim_element: Option<TrimElement>,
    extended_object_element: Option<ExtendedObjectElement>,
    headphone_element: Option<HeadphoneElement>,
    object_description_element: Option<ObjectDescriptionElement>,
    bed_object_element: Option<BedObjectElement>,
}

impl Default for OAMDParserState {
    fn default() -> Self {
        Self {
            object_count: 0,
            program_assignment: ProgramAssignment::default(),
            b_alternate_object_data_present: false,
            prev_object_gain: [0; MAX_OBJ_INFO_BLOCKS],
            prev_object_basic_info: ObjectBasicInfo::default(),
            prev_object_render_info: ObjectRenderInfo::default(),
            object_element: None,
            trim_element: None,
            extended_object_element: None,
            headphone_element: None,
            object_description_element: None,
            bed_object_element: None,
        }
    }
}

/// Speaker bed assignment for spatial audio channels
#[derive(Copy, Clone, Debug, Default, PartialEq)]
#[repr(C)]
pub struct BedAssignment(pub [bool; 17]);

impl BedAssignment {
    pub fn from_non_std(value: u32) -> Self {
        let mut ret = Self::default();

        ret.0
            .iter_mut()
            .enumerate()
            .for_each(|(i, x)| *x = (value >> i) & 1 == 1);

        ret
    }

    pub fn from_std(value: u16) -> Self {
        let mut ret = Self::default();

        for (i, &bed) in STD_BED_LIST.iter().enumerate() {
            if (value >> i) & 1 == 1 {
                for &n in bed {
                    ret.0[n] = true
                }
            }
        }

        ret
    }

    pub fn with_lfe_only() -> Self {
        let mut ret = Self::default();
        ret.0[SpeakerLabels::LFE as usize] = true;

        ret
    }

    pub fn to_index_vec(&self) -> Vec<usize> {
        self.0
            .iter()
            .enumerate()
            .filter_map(|(i, &sign)| if sign { Some(i) } else { None })
            .collect()
    }

    pub fn is_lfe_only(&self) -> bool {
        *self == Self::with_lfe_only()
    }

    pub fn count_beds(&self) -> usize {
        self.0.iter().fold(0, |count, &sign| count + sign as usize)
    }
}

pub const STD_BED_LIST: [&[usize]; 10] = [
    &[0, 1],
    &[2],
    &[3],
    &[4, 5],
    &[6, 7],
    &[8, 9],
    &[10, 11],
    &[12, 13],
    &[14, 15],
    &[16],
];

/// Indexed by the 3-bit `intermediate_spatial_format_idx`. Indices 6 and 7 are
/// reserved and yield zero objects.
pub const ISF_COUNT_LIST: [usize; 8] = [4, 8, 10, 14, 15, 30, 0, 0];

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ProgramAssignment {
    pub b_bed_chan_distribute: bool,
    pub bed_assignment: Vec<BedAssignment>,
    pub num_bed_objects: usize,
    pub num_isf_objects: usize,
    pub num_dynamic_objects: usize,
}

impl ProgramAssignment {
    pub fn b_dyn_object_only_program(&self) -> bool {
        // TS 103 420 5.6.0.5: dynamic objects plus at most an LFE, nothing else.
        self.num_bed_objects <= 1 && self.num_isf_objects == 0 && self.num_dynamic_objects > 0
    }

    pub fn beds_or_isf_count(&self) -> usize {
        self.num_bed_objects + self.num_isf_objects
    }

    fn read(state: &mut OAMDParserState, reader: &mut SliceReader) -> Result<Self> {
        let mut prog = Self::default();

        // b_dyn_object_only_program
        if reader.get()? {
            prog.num_dynamic_objects = state.object_count;

            let b_lfe_present = reader.get()?;

            if b_lfe_present {
                prog.bed_assignment.push(BedAssignment::with_lfe_only());
                prog.num_dynamic_objects -= 1;
            }
        } else {
            let content_description = reader.get_n::<u8>(4)?;

            // object(s) with speaker-anchored coordinate(s) (bed objects)
            if content_description & 1 != 0 {
                prog.b_bed_chan_distribute = reader.get()?;

                // b_multiple_bed_instances_present
                let num_bed_instances = if reader.get()? {
                    // num_bed_instances_bits
                    reader.get_n::<u8>(3)? + 2
                } else {
                    1
                };

                for _ in 0..num_bed_instances {
                    // b_lfe_only
                    let bed_assignment = if reader.get()? {
                        BedAssignment::with_lfe_only()
                    } else {
                        // b_standard_chan_assign
                        if reader.get()? {
                            BedAssignment::from_std(reader.get_n::<u16>(10)?)
                        } else {
                            BedAssignment::from_non_std(reader.get_n::<u32>(17)?)
                        }
                    };

                    prog.bed_assignment.push(bed_assignment)
                }
            }

            // intermediate spatial format (ISF)
            if content_description & 2 != 0 {
                let intermediate_spatial_format_idx = reader.get_n::<u8>(3)?;
                prog.num_isf_objects = ISF_COUNT_LIST[intermediate_spatial_format_idx as usize];
            }

            // object(s) with room-anchored or screen-anchored coordinates
            if content_description & 4 != 0 {
                let mut num_dynamic_objects = reader.get_n::<u8>(5)?;
                if num_dynamic_objects == 31 {
                    num_dynamic_objects += reader.get_n::<u8>(7)?;
                };

                prog.num_dynamic_objects = num_dynamic_objects as usize + 1;
            }

            // reserved
            if content_description & 8 != 0 {
                let reserved_data_size = (reader.get_n::<u32>(4)? + 1) << 3;
                reader.skip_n(reserved_data_size)?;
            }
        };

        for &a in prog.bed_assignment.iter() {
            prog.num_bed_objects += a.count_beds();
        }

        Ok(prog)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ObjectAudioMetadataPayload {
    pub evo_sample_offset: u64,
    pub oamd_version: u8,
    pub object_count: usize,
    pub program_assignment: ProgramAssignment,
    pub b_alternate_object_data_present: bool,

    pub object_element: Option<ObjectElement>,
    pub trim_element: Option<TrimElement>,
    pub extended_object_element: Option<ExtendedObjectElement>,
    pub headphone_element: Option<HeadphoneElement>,
    pub object_description_element: Option<ObjectDescriptionElement>,
    pub bed_object_element: Option<BedObjectElement>,
    pub oa_element_md: Vec<OAElementMD>,
}

impl ObjectAudioMetadataPayload {
    pub fn read(bytes: &[u8]) -> Result<Self> {
        let state = &mut OAMDParserState::default();
        let reader = &mut SliceReader::new(bytes);

        let mut oamd_version = reader.get_n::<u8>(2)?;

        if oamd_version == 3 {
            oamd_version += reader.get_n::<u8>(3)?;
        }

        // Field layout below is only defined for version 0. A payload of another version is
        // returned without elements, not as an error: the access unit carrying it still decodes,
        // and its objects keep the metadata they had.
        if oamd_version != 0 {
            warn!("OAMD version {oamd_version} is not read; its payload is skipped");
            return Ok(Self {
                oamd_version,
                ..Default::default()
            });
        }

        let mut object_count_bits = reader.get_n::<u8>(5)?;

        if object_count_bits == 31 {
            object_count_bits += reader.get_n::<u8>(7)?;
        }

        let object_count = (object_count_bits + 1) as usize;
        state.object_count = object_count;

        let program_assignment = ProgramAssignment::read(state, reader)?;
        state.program_assignment = program_assignment.clone();

        let b_alternate_object_data_present = reader.get()?;
        state.b_alternate_object_data_present = b_alternate_object_data_present;

        let mut oa_element_count = reader.get_n::<u8>(4)?;
        if oa_element_count == 15 {
            oa_element_count += reader.get_n::<u8>(5)?;
        }

        let mut oa_element_md = Vec::with_capacity(oa_element_count as usize);
        for index in 0..oa_element_count {
            // A truncated element is read up to the end of the payload; the ones after it are lost.
            if reader.available()? == 0 {
                warn!(
                    "OAMD payload ends before its last {} elements",
                    oa_element_count as usize - oa_element_md.len()
                );
                break;
            }
            oa_element_md.push(OAElementMD::read(
                state,
                reader,
                index + 1 == oa_element_count,
            )?);
        }

        let payload = Self {
            evo_sample_offset: 0,
            oamd_version,
            object_count,
            program_assignment,
            b_alternate_object_data_present,
            object_element: state.object_element.clone(),
            trim_element: state.trim_element.clone(),
            extended_object_element: state.extended_object_element.clone(),
            headphone_element: state.headphone_element.clone(),
            object_description_element: state.object_description_element.clone(),
            bed_object_element: state.bed_object_element.clone(),
            oa_element_md,
        };

        Ok(payload)
    }

    pub fn get_damf_pos(&self) -> Vec<Vec<[f64; 3]>> {
        let mut damf_pos = vec![vec![]; self.object_count];

        if let Some(object_element) = &self.object_element {
            for (object_index, object_data) in object_element.object_data.iter().enumerate() {
                for block in object_data {
                    damf_pos[object_index].push(block.object_render_info.pos3d);
                }
            }
        }

        if let Some(extended_object_element) = &self.extended_object_element {
            for (object_index, object) in extended_object_element
                .ext_prec_pos_block
                .iter()
                .enumerate()
            {
                let pos3d_object = &mut damf_pos[object_index];
                for (block_index, block) in object.iter().enumerate() {
                    let pos3d = &mut pos3d_object[block_index];

                    pos3d[0] += block.ext_prec_pos3d_x;
                    pos3d[1] += block.ext_prec_pos3d_y;
                    pos3d[2] += block.ext_prec_pos3d_z;
                }
            }
        }

        damf_pos.iter_mut().for_each(|pos3d_object| {
            pos3d_object.iter_mut().for_each(|pos3d| {
                pos3d[0] = (pos3d[0].clamp(0.0, 1.0) - 0.5) * 2.0;
                pos3d[1] = (0.5 - pos3d[1].clamp(0.0, 1.0)) * 2.0;
                pos3d[2] = pos3d[2].clamp(-1.0, 1.0);
            })
        });

        damf_pos
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct OAElementMD {
    pub oa_element_id_idx: u8,
    pub alternate_object_data_id_idx: Option<u8>,
    pub b_discard_unknown_element: bool,
    // pub reserved_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
#[repr(u8)]
pub enum OAElementType {
    Object = 1,
    Trim = 2,
    Headphone = 3,
    ObjectDescription = 4,
    ExtendObject = 5,
    BedObject = 6,
    Reserved(u8),
}

impl OAElementType {
    pub fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Object,
            2 => Self::Trim,
            3 => Self::Headphone,
            4 => Self::ObjectDescription,
            5 => Self::ExtendObject,
            6 => Self::BedObject,
            _ => Self::Reserved(n),
        }
    }
}

impl OAElementMD {
    /// One element (TS 103 420 5.5.5). Its declared size is where the next element starts
    /// (5.6.4.3), so an element with one after it is read from that size alone: one whose fields run
    /// past it is malformed and dropped, and the next element still starts where the size says, as
    /// do elements of a type this parser does not read. The `last` element has nothing after it for
    /// its size to place, and is read up to the end of the payload: some TrueHD encoders declare the
    /// programme's first object element shorter than the fields they write, and its objects are all
    /// there.
    fn read(state: &mut OAMDParserState, reader: &mut SliceReader, last: bool) -> Result<Self> {
        let oa_element_id_idx = reader.get_n::<u8>(4)?;
        let oa_element_size_bits = reader.get_variable_bits_max(4, 4)? as u64;
        let oa_element_size = (oa_element_size_bits + 1) << 3;

        let available = reader.available()?;
        if oa_element_size > available {
            warn!(
                "oa_element {oa_element_id_idx} declares {oa_element_size} bits, {available} remain; read to the end of the payload"
            );
        }
        let body = reader.take(if last {
            available
        } else {
            oa_element_size.min(available)
        })?;
        let element_reader = &mut SliceReader::new(&body);

        let mut md = OAElementMD {
            oa_element_id_idx,
            ..Default::default()
        };
        if let Err(e) = md.read_body(state, element_reader) {
            warn!("Dropping oa_element {oa_element_id_idx}: {e}");
        } else if element_reader.position()? > oa_element_size {
            warn!(
                "oa_element {oa_element_id_idx} declares {oa_element_size} bits and takes {}; the last element, read to the end of the payload",
                element_reader.position()?
            );
        }

        trace!(
            "OAMD Element ID: {}, size: {}, read: {}",
            md.oa_element_id_idx,
            oa_element_size,
            element_reader.position().unwrap_or(0)
        );

        Ok(md)
    }

    fn read_body(&mut self, state: &mut OAMDParserState, reader: &mut SliceReader) -> Result<()> {
        if state.b_alternate_object_data_present {
            self.alternate_object_data_id_idx = Some(reader.get_n::<u8>(4)?);
        }

        self.b_discard_unknown_element = reader.get()?;

        // TS 103 420 table 27: 0 is the element itself; 1-15 are alternative data of a kind no
        // version defines yet, which must not replace it.
        let element_type = match self.alternate_object_data_id_idx {
            Some(id) if id != 0 => {
                warn!(
                    "Skipping oa_element {} with reserved alternate data id {id}",
                    self.oa_element_id_idx
                );
                return Ok(());
            }
            _ => OAElementType::from_u8(self.oa_element_id_idx),
        };

        match element_type {
            // An element type the payload has already given is a duplicate: TS 103 420 tells
            // elements of one type apart only by alternate_object_data_id (table 27), and those
            // already left with a non-zero one. The first stands.
            OAElementType::Object if state.object_element.is_some() => {
                warn!("Duplicate object element, keeping the first")
            }
            OAElementType::Trim if state.trim_element.is_some() => {
                warn!("Duplicate trim element, keeping the first")
            }
            OAElementType::ExtendObject if state.extended_object_element.is_some() => {
                warn!("Duplicate extended object element, keeping the first")
            }
            OAElementType::Headphone if state.headphone_element.is_some() => {
                warn!("Duplicate headphone element, keeping the first")
            }
            OAElementType::ObjectDescription if state.object_description_element.is_some() => {
                warn!("Duplicate object description element, keeping the first")
            }
            OAElementType::BedObject if state.bed_object_element.is_some() => {
                warn!("Duplicate bed object element, keeping the first")
            }
            OAElementType::Object => {
                state.object_element = Some(ObjectElement::read(state, reader)?);
            }
            OAElementType::Trim => {
                state.trim_element = Some(TrimElement::read(state, reader)?);
            }
            OAElementType::ExtendObject => {
                state.extended_object_element = Some(ExtendedObjectElement::read(state, reader)?);
            }
            OAElementType::Headphone => {
                state.headphone_element = Some(HeadphoneElement::read(state, reader)?);
            }
            OAElementType::BedObject => {
                state.bed_object_element =
                    Some(BedObjectElement::read(&state.program_assignment, reader)?);
            }
            OAElementType::ObjectDescription => {
                state.object_description_element =
                    Some(ObjectDescriptionElement::read(state, reader)?);
            }
            OAElementType::Reserved(id) => {
                warn!("Unimplemented oa_element_md type {id}, please submit a sample")
            }
        }

        Ok(())
    }
}

pub type ObjectData = Vec<ObjectInfoBlock>;

#[derive(Clone, Debug)]
#[repr(C)]
pub struct ObjectElement {
    pub md_update_info: MDUpdateInfo,
    pub b_reserved_data_not_present: bool,
    pub reserved_data: u8,
    pub object_data: Vec<ObjectData>,
}

impl Default for ObjectElement {
    fn default() -> Self {
        Self {
            md_update_info: MDUpdateInfo::default(),
            b_reserved_data_not_present: false,
            reserved_data: 32, // TODO: investigate
            object_data: Vec::new(),
        }
    }
}

impl ObjectElement {
    fn read(state: &mut OAMDParserState, reader: &mut SliceReader) -> Result<Self> {
        let mut element = Self {
            md_update_info: MDUpdateInfo::read(reader)?,
            b_reserved_data_not_present: reader.get()?,
            object_data: Vec::with_capacity(state.object_count),
            ..Default::default()
        };

        if !element.b_reserved_data_not_present {
            element.reserved_data = reader.get_n::<u8>(5)?;
        }

        let block_count = element.md_update_info.num_obj_info_blocks;

        for object_index in 0..state.object_count {
            let mut object_data = ObjectData::default();

            for block_index in 0..block_count {
                object_data.push(ObjectInfoBlock::read(
                    state,
                    reader,
                    object_index,
                    block_index,
                )?);
            }
            element.object_data.push(object_data.clone());
        }

        Ok(element)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct MDUpdateInfo {
    pub sample_offset: usize,
    pub num_obj_info_blocks: usize,
    pub block_update_info: Vec<BlockUpdateInfo>,
}

impl MDUpdateInfo {
    pub(crate) fn read(reader: &mut SliceReader) -> Result<Self> {
        // sample_offset_code
        let sample_offset = match reader.get_n::<u8>(2)? {
            0 => 0,
            // sample_offset_idx
            1 => match reader.get_n::<u8>(2)? {
                0 => 8,
                1 => 16,
                2 => 18,
                3 => 24,
                _ => unreachable!(),
            },
            // sample_offset_bits
            2 => reader.get_n::<u8>(5)? as usize,
            // reserved (table 22): nothing follows, and there is no offset
            _ => 0,
        };

        let num_obj_info_blocks = (reader.get_n::<u8>(3)? + 1) as usize;

        let mut info = Self {
            sample_offset,
            num_obj_info_blocks,
            block_update_info: Vec::with_capacity(num_obj_info_blocks),
        };

        for _block in 0..num_obj_info_blocks {
            info.block_update_info.push(BlockUpdateInfo::read(reader)?);
        }

        Ok(info)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct BlockUpdateInfo {
    pub block_offset_factor_bits: u8,
    pub ramp_duration_code: u8,
    pub ramp_duration: u16,
}

impl BlockUpdateInfo {
    pub const RAMP_DURATION_LIST: [u16; 16] = [
        32, 64, 128, 256, 320, 480, 1000, 1001, 1024, 1600, 1601, 1602, 1920, 2000, 2002, 2048,
    ];

    pub(crate) fn read(reader: &mut SliceReader) -> Result<Self> {
        let mut info = Self {
            block_offset_factor_bits: reader.get_n(6)?,
            ramp_duration_code: reader.get_n(2)?,
            ..Default::default()
        };

        info.ramp_duration = match info.ramp_duration_code {
            0 => 0,
            1 => 512,
            2 => 1536,
            3 => {
                // b_use_ramp_duration_idx
                if reader.get()? {
                    // ramp_duration_idx, four bits, so it cannot leave the table
                    Self::RAMP_DURATION_LIST[reader.get_n::<u8>(4)? as usize]
                } else {
                    // ramp_duration_bits
                    reader.get_n(11)?
                }
            }
            _ => unreachable!(),
        };

        Ok(info)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ObjectInfoBlock {
    pub b_object_not_active: bool,
    pub object_basic_info: ObjectBasicInfo,
    pub b_object_in_bed_or_isf: bool,
    pub object_render_info: ObjectRenderInfo,
}

impl ObjectInfoBlock {
    fn read(
        state: &mut OAMDParserState,
        reader: &mut SliceReader,
        object_index: usize,
        block_index: usize,
    ) -> Result<Self> {
        let mut info = Self {
            b_object_not_active: reader.get()?,
            ..Default::default()
        };

        let object_basic_info_status_idx = if info.b_object_not_active {
            0
        } else if block_index == 0 {
            1
        } else {
            reader.get_n::<u8>(2)?
        };

        let prev_object_basic_info = if block_index == 0 {
            ObjectBasicInfo::default()
        } else {
            state.prev_object_basic_info.clone()
        };

        info.object_basic_info = match object_basic_info_status_idx {
            0 => ObjectBasicInfo::default(),
            1 | 3 => ObjectBasicInfo::read(
                &prev_object_basic_info,
                state,
                reader,
                object_index,
                block_index,
                object_basic_info_status_idx,
            )?,
            _ => prev_object_basic_info,
        };

        state.prev_object_basic_info = info.object_basic_info.clone();
        // Table 18's object_gain_idx 3 means the previous object's gain in this block, whichever
        // way that object's gain was arrived at.
        state.prev_object_gain[block_index] = info.object_basic_info.object_gain;

        info.b_object_in_bed_or_isf = object_index < state.program_assignment.beds_or_isf_count();

        let object_render_info_status_idx = if info.b_object_not_active {
            0
        } else if !info.b_object_in_bed_or_isf {
            if block_index == 0 {
                1
            } else {
                reader.get_n(2)?
            }
        } else {
            0
        };

        let prev_object_render_info = if block_index == 0 {
            ObjectRenderInfo::default()
        } else {
            state.prev_object_render_info.clone()
        };

        info.object_render_info = match object_render_info_status_idx {
            0 => ObjectRenderInfo::default(),
            1 | 3 => ObjectRenderInfo::read(
                &prev_object_render_info,
                reader,
                object_render_info_status_idx,
                block_index,
            )?,
            _ => prev_object_render_info,
        };

        state.prev_object_render_info = info.object_render_info.clone();

        // b_additional_table_data_exists
        if reader.get()? {
            // additional_table_data_size_bits
            let additional_table_data_size = (reader.get_n::<u32>(4)? + 1) << 3;
            reader.skip_n(additional_table_data_size)?;
        }

        Ok(info)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct ObjectBasicInfo {
    pub object_gain: i8,
    pub object_priority: f64,
}

impl Default for ObjectBasicInfo {
    fn default() -> Self {
        Self {
            object_gain: GAIN_MINUS_INFINITY,
            object_priority: 0.0,
        }
    }
}

impl ObjectBasicInfo {
    pub fn gain_string(&self) -> String {
        match self.object_gain {
            GAIN_MINUS_INFINITY => "-inf".to_string(),
            _ => format!("{}", self.object_gain),
        }
    }

    fn read(
        prev: &Self,
        state: &mut OAMDParserState,
        reader: &mut SliceReader,
        object_index: usize,
        block_index: usize,
        object_basic_info_status_idx: u8,
    ) -> Result<Self> {
        let mut basic = prev.clone();

        let obj_basic_info_bits = if object_basic_info_status_idx == 1 {
            3u8
        } else {
            reader.get_n(2)?
        };

        if obj_basic_info_bits & 1 != 0 {
            let prev_object_gain = if object_index == 0 {
                0
            } else {
                state.prev_object_gain[block_index]
            };

            // object_gain_idx
            basic.object_gain = match reader.get_n::<u8>(2)? {
                0 => 0,
                1 => GAIN_MINUS_INFINITY,
                2 => {
                    let object_gain_idx = reader.get_n::<u8>(6)?;

                    if object_gain_idx <= 14 {
                        15 - object_gain_idx as i8
                    } else {
                        14 - object_gain_idx as i8
                    }
                }
                3 => prev_object_gain,
                _ => unreachable!(),
            };
        }

        if obj_basic_info_bits & 2 != 0 {
            // b_default_object_priority
            basic.object_priority = if reader.get()? {
                1.0
            } else {
                reader.get_n::<u8>(5)? as f64 / 32.0
            }
        }

        Ok(basic)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct ObjectRenderInfo {
    pub b_differential_position_specified: bool,
    /// Standard-precision position, unclamped: TS 103 420 5.6.1.1.8-14 clamp once, after the
    /// extended precision refinement is added, so whoever adds it clamps.
    pub pos3d: [f64; 3],
    /// The standard-precision codes behind `pos3d` (x and y in 62nds, z signed in 15ths), which
    /// the next block's differential position applies to.
    pub pos3d_code: [i32; 3],
    pub b_object_distance_specified: bool,
    /// distance_factor, from [`DISTANCE_FACTORS`] at parse time: a ratio to the room boundary, not
    /// metres. `None` when unspecified, `f64::INFINITY` for objects at infinity.
    pub distance_factor: Option<f64>,
    pub zone_constraints_idx: u8,
    pub b_enable_elevation: bool,
    pub object_size: [f64; 3],
    pub b_object_use_screen_ref: bool,
    pub screen_factor: f64,
    pub depth_factor: f64,
    pub b_object_snap: bool,
}

impl Default for ObjectRenderInfo {
    fn default() -> Self {
        Self {
            b_differential_position_specified: false,
            pos3d: [0.5, 0.5, 0.0],
            pos3d_code: [31, 31, 0],
            b_object_distance_specified: false,
            distance_factor: None,
            zone_constraints_idx: 0,
            b_enable_elevation: true,
            object_size: [0.0, 0.0, 0.0],
            b_object_use_screen_ref: false,
            screen_factor: 0.0,
            depth_factor: 0.25,
            b_object_snap: false,
        }
    }
}

impl ObjectRenderInfo {
    fn read(
        prev: &Self,
        reader: &mut SliceReader,
        object_render_info_status_idx: u8,
        block_index: usize,
    ) -> Result<Self> {
        let mut render = prev.clone();
        // obj_render_info[] is numbered in reading order, [0] the first bit read, as table 31 has it
        // (position [3], the last bit), and as obj_basic_info[] and trim_balance_presence[] are
        // numbered in their syntax and tables alike. The syntax on p32 numbers it the other way
        // round. The reading order is the one real payloads parse with: their trim elements read
        // to within their size with zero padding this way, and not the other.
        let object_render_info_bits = if object_render_info_status_idx == 1 {
            15
        } else {
            reader.get_n::<u8>(4)?
        };

        if object_render_info_bits & 1 != 0 {
            render.b_differential_position_specified = if block_index == 0 {
                false
            } else {
                reader.get()?
            };

            render.pos3d_code = if render.b_differential_position_specified {
                let [x, y, z] = prev.pos3d_code;
                [
                    x + reader.get_s::<i8>(3)? as i32,
                    y + reader.get_s::<i8>(3)? as i32,
                    z + reader.get_s::<i8>(3)? as i32,
                ]
            } else {
                let x = reader.get_n::<u8>(6)? as i32;
                let y = reader.get_n::<u8>(6)? as i32;
                let sign_z = if reader.get()? { 1 } else { -1 };
                [x, y, sign_z * reader.get_n::<u8>(4)? as i32]
            };
            let [x, y, z] = render.pos3d_code;
            render.pos3d = [x as f64 / 62.0, y as f64 / 62.0, z as f64 / 15.0];

            render.b_object_distance_specified = reader.get()?;

            render.distance_factor = if render.b_object_distance_specified {
                // b_object_at_infinity
                if reader.get()? {
                    Some(f64::INFINITY)
                } else {
                    let idx = reader.get_n::<u8>(4)? as usize;
                    Some(DISTANCE_FACTORS[idx])
                }
            } else {
                None
            };
        }

        if object_render_info_bits & 2 != 0 {
            // 6 and 7 are reserved (table 20); they are read and stored as 0, no constraint.
            render.zone_constraints_idx = match reader.get_n(3)? {
                6 | 7 => 0,
                idx => idx,
            };
            render.b_enable_elevation = reader.get()?;
        }

        if object_render_info_bits & 4 != 0 {
            // object_size_idx
            render.object_size = match reader.get_n::<u8>(2)? {
                1 => {
                    let object_size = reader.get_n::<u8>(5)? as f64 / 31.0;
                    [object_size; 3]
                }
                2 => {
                    // seems not allowed in Atmos
                    let width = reader.get_n::<u8>(5)? as f64 / 31.0;
                    let depth = reader.get_n::<u8>(5)? as f64 / 31.0;
                    let height = reader.get_n::<u8>(5)? as f64 / 31.0;
                    [width, depth, height]
                }
                // 3 => // reserved
                _ => [0.0; 3],
            };
        }

        if object_render_info_bits & 8 != 0 {
            render.b_object_use_screen_ref = reader.get()?;

            if render.b_object_use_screen_ref {
                render.screen_factor = (reader.get_n::<u8>(3)? + 1) as f64 / 8.0;
                render.depth_factor = DEPTH_FACTORS[reader.get_n::<u8>(2)? as usize];
            } else {
                render.screen_factor = 0.0
            }
        }

        render.b_object_snap = reader.get()?;

        Ok(render)
    }
}

pub const NUM_TRIM_CONFIGS: usize = 9;

/// Maps a 4-bit distance_factor_idx to distance_factor (TS 103 420 table 15): how many times
/// farther from the room's centre the object is than the room boundary in its direction, a ratio
/// and not a distance in metres.
#[rustfmt::skip]
pub const DISTANCE_FACTORS: [f64; 16] = [
    1.1, 1.3, 1.6, 2.0, 2.5, 3.2, 4.0, 5.0, 6.3, 7.9, 10.0, 12.6, 15.8, 20.0, 25.1, 50.1,
];

/// Maps a 2-bit depth_factor_idx to depth_factor (TS 103 420 table 16).
pub const DEPTH_FACTORS: [f64; 4] = [0.25, 0.5, 1.0, 2.0];

/// trim_centre in dB (TS 103 420 table 35). Code 14 is -15 dB as oamd 0.1.0 has it (Q12 0x02d8),
/// not the table's -16; -15 continues the table's 1.5 dB steps.
#[rustfmt::skip]
pub const TRIM_LUT: [f64; 16] = [
    6.0, 3.0, 1.5, 0.75, -0.75, -1.5, -3.0, -4.5, -6.0, -7.5, -9.0, -10.5, -12.0, -13.5, -15.0, -36.0,
];

/// trim_surround and trim_height in dB (tables 36 and 37): codes 0-3 are reserved, None.
#[rustfmt::skip]
pub const TRIM_ATTENUATION_LUT: [Option<f64>; 16] = [
    None, None, None, None, Some(-0.75), Some(-1.5), Some(-3.0), Some(-4.5), Some(-6.0), Some(-7.5),
    Some(-9.0), Some(-10.5), Some(-12.0), Some(-13.5), Some(-15.0), Some(-36.0),
];

// TODO: TrimElement & ExtendObjectElement
/// One object's headphone rendering intent at one block, from `oa_element_id` 3.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct HeadphoneBlock {
    /// 0 bypass (stereo), 1 near, 2 far, 3 mid: AC-4's hp_render_mode_obj (TS 103 190-2
    /// V1.3.1 table 121), the same element in that codec. TS 103 420 leaves element 3 reserved.
    pub hp_render_mode: u8,
    pub hp_head_track_disable: bool,
}

/// Headphone element, `oa_element_id` 3.
///
/// Four modes: three state one rendering intent for the whole payload, and the fourth
/// codes it per object and block. A mode above 3 is not defined and reads nothing further,
/// leaving the element empty.
#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct HeadphoneElement {
    pub hp_mode: u8,
    pub num_obj_info_blocks: usize,
    /// Indexed by object, then by block. Empty where the mode defines nothing.
    pub headphone_data: Vec<Vec<HeadphoneBlock>>,
}

impl HeadphoneElement {
    fn read(state: &mut OAMDParserState, reader: &mut SliceReader) -> Result<Self> {
        let mut element = Self {
            hp_mode: reader.get_n(3)?,
            num_obj_info_blocks: reader.get_n::<u8>(3)? as usize + 1,
            ..Default::default()
        };

        let (objects, blocks) = (state.object_count, element.num_obj_info_blocks);

        // A mode this does not define reads nothing more, and the element carries no
        // rendering intent rather than a guessed one.
        let (mode, head_track_disable) = match element.hp_mode {
            0 => (0, true),
            1 | 2 => (element.hp_mode, reader.get()?),
            3 => {
                element.headphone_data = Self::read_per_object(state, reader, blocks)?;
                return Ok(element);
            }
            _ => return Ok(element),
        };

        element.headphone_data = vec![
            vec![
                HeadphoneBlock {
                    hp_render_mode: mode,
                    hp_head_track_disable: head_track_disable,
                };
                blocks
            ];
            objects
        ];

        Ok(element)
    }

    /// Mode 3 codes every object, except that a run of consecutive ISF objects is coded
    /// once and repeated: the objects of one ISF group share a rendering intent.
    fn read_per_object(
        state: &mut OAMDParserState,
        reader: &mut SliceReader,
        blocks: usize,
    ) -> Result<Vec<Vec<HeadphoneBlock>>> {
        let prog = &state.program_assignment;
        let isf = prog.num_bed_objects..prog.num_bed_objects + prog.num_isf_objects;

        let mut data: Vec<Vec<HeadphoneBlock>> = Vec::with_capacity(state.object_count);

        for object_index in 0..state.object_count {
            if isf.contains(&object_index)
                && object_index != isf.start
                && let Some(first) = data.get(isf.start).cloned()
            {
                data.push(first);
                continue;
            }

            let mut object = Vec::with_capacity(blocks);
            for _ in 0..blocks {
                object.push(HeadphoneBlock {
                    hp_render_mode: reader.get_n::<u8>(2)?,
                    hp_head_track_disable: reader.get()?,
                });
            }
            data.push(object);
        }

        Ok(data)
    }
}

/// Object description element, `oa_element_id` 4, which says which objects are dialogue.
#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ObjectDescriptionElement {
    pub num_obj_info_blocks: usize,
    pub b_present: bool,
    pub sub_mode: u8,
    /// Indexed by object, then by block. Empty where the element states nothing.
    pub object_dialog_indication: Vec<Vec<u8>>,
}

impl ObjectDescriptionElement {
    /// The 2-bit code of sub-mode 1 is not the indication itself.
    const DIALOG_MAP: [u8; 4] = [2, 1, 3, 0];

    fn read(state: &mut OAMDParserState, reader: &mut SliceReader) -> Result<Self> {
        let mut element = Self {
            num_obj_info_blocks: reader.get_n::<u8>(3)? as usize + 1,
            b_present: reader.get()?,
            ..Default::default()
        };

        if !element.b_present {
            return Ok(element);
        }

        element.sub_mode = reader.get_n(2)?;
        let (objects, blocks) = (state.object_count, element.num_obj_info_blocks);

        match element.sub_mode {
            0 => {
                element.object_dialog_indication = Self::per_object(
                    objects,
                    blocks,
                    |r| Ok(if r.get()? { 1 } else { 2 }),
                    reader,
                )?;
            }
            1 => {
                let b_extra = reader.get()?;
                element.object_dialog_indication = Self::per_object(
                    objects,
                    blocks,
                    |r| {
                        let code = r.get_n::<u8>(2)? as usize;
                        // Only that one code carries the five further bits, whose meaning is
                        // not stated anywhere this was read from.
                        if code == 2 && b_extra {
                            r.skip_n(5)?;
                        }
                        Ok(Self::DIALOG_MAP[code])
                    },
                    reader,
                )?;
            }
            // Reserved data rather than indications, so every object reads as 0.
            _ => {
                let mut size = reader.get_n::<u32>(5)? + 1;
                if reader.get()? {
                    size += reader.get_variable_bits_max(2, 3)?;
                }

                element.object_dialog_indication = vec![vec![0; blocks]; objects];
                reader.skip_n(size * 8)?;
            }
        }

        Ok(element)
    }

    fn per_object(
        objects: usize,
        blocks: usize,
        mut one: impl FnMut(&mut SliceReader) -> Result<u8>,
        reader: &mut SliceReader,
    ) -> Result<Vec<Vec<u8>>> {
        (0..objects)
            .map(|_| (0..blocks).map(|_| one(reader)).collect())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct DownmixRoute {
    /// 0=front, 1=side, 2=rear surround (only for the three-way tool).
    pub destination: u8,
    pub gain_code: u8,
}

/// Bed object element, `oa_element_id` 6, which says how a bed folds down to fewer outputs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct BedObjectElement {
    pub is_channels: bool,
    /// Top front, top back, top pair, back pair, wides and LFE, derived from the
    /// program assignment rather than read from the element.
    pub presence: [bool; 6],
    /// Center and surround gain codes.
    pub loro: Option<[u8; 2]>,
    pub ltrt: Option<[u8; 2]>,
    pub preferred_downmix: Option<u8>,
    pub lfe_gain: Option<u8>,
    pub custom_downmix: bool,
    pub wide_to_front: Option<u8>,
    pub back4_to_back2: Option<u8>,
    /// The top pair, top back and top front routes, in that order.
    pub to_7_1: [Option<DownmixRoute>; 3],
    pub to_5_1: [Option<DownmixRoute>; 3],
    pub top4_to_top2: Option<u8>,
}

impl BedObjectElement {
    fn read(program: &ProgramAssignment, reader: &mut SliceReader) -> Result<Self> {
        let mask = program.bed_assignment.iter().fold(0u32, |mask, bed| {
            bed.0
                .iter()
                .enumerate()
                .fold(mask, |m, (i, &on)| m | ((on as u32) << i))
        });
        let mut element = Self {
            presence: [0x300, 0x3000, 0xc00, 0xc0, 0xc000, 0x10008].map(|m| mask & m != 0),
            ..Self::default()
        };
        // The element's own reader ends at its declared size.
        let mut read = |n: u32| -> Result<u8> { Ok(reader.get_n::<u8>(n)?) };
        element.is_channels = read(1)? != 0;
        if !element.is_channels {
            return Ok(element);
        }
        if read(1)? != 0 {
            element.loro = Some([read(3)?, read(3)?]);
        }
        if read(1)? != 0 {
            element.ltrt = Some([read(3)?, read(3)?]);
        }
        // The surround code has two reserved values, unlike the center code.
        for gains in [element.loro, element.ltrt].into_iter().flatten() {
            if gains[1] < 2 {
                warn!("Reserved bed object surround gain code {}", gains[1]);
            }
        }
        if element.loro.is_some() || element.ltrt.is_some() {
            element.preferred_downmix = Some(read(2)?);
        }
        if element.presence[5] && read(1)? != 0 {
            element.lfe_gain = Some(read(5)?);
        }
        element.custom_downmix = read(1)? != 0;
        if !element.custom_downmix {
            return Ok(element);
        }
        if element.presence[4] && read(1)? != 0 {
            element.wide_to_front = Some(read(3)?);
        }
        if element.presence[3] && read(1)? != 0 {
            element.back4_to_back2 = Some(read(3)?);
        }
        for (i, p) in [2, 1, 0].into_iter().enumerate() {
            if !element.presence[p] {
                continue;
            }
            if read(1)? != 0 {
                let destination = if read(1)? != 0 {
                    0
                } else if read(1)? != 0 {
                    1
                } else {
                    2
                };
                element.to_7_1[i] = Some(DownmixRoute {
                    destination,
                    gain_code: read(3)?,
                });
            }
            if read(1)? != 0 {
                let destination = if read(1)? != 0 { 0 } else { 1 };
                element.to_5_1[i] = Some(DownmixRoute {
                    destination,
                    gain_code: read(3)?,
                });
            }
        }
        if (element.presence[0] || element.presence[1]) && read(1)? != 0 {
            element.top4_to_top2 = Some(read(3)?);
        }
        Ok(element)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct TrimElement {
    pub warp_mode: u8,
    pub reserved: u8,
    pub global_trim_mode: u8,
    pub trims: [Option<Trim>; NUM_TRIM_CONFIGS],
    pub b_disable_trim_per_obj: bool,
    pub b_disable_trim: Vec<bool>,
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct Trim {
    pub b_default_trim: bool,
    pub b_disable_trim: bool,

    pub trim_centre: Option<f64>,
    pub trim_surround: Option<f64>,
    pub trim_height: Option<f64>,
    pub bal3d_y_tb: Option<f64>,
    pub bal3d_y_lis: Option<f64>,
}

impl Default for Trim {
    fn default() -> Trim {
        Trim {
            b_default_trim: true,
            b_disable_trim: false,
            trim_centre: None,
            trim_surround: None,
            trim_height: None,
            bal3d_y_tb: None,
            bal3d_y_lis: None,
        }
    }
}

impl TrimElement {
    fn read(state: &OAMDParserState, reader: &mut SliceReader) -> Result<Self> {
        let mut element = Self {
            warp_mode: reader.get_n(2)?,
            reserved: reader.get_n(2)?,
            global_trim_mode: reader.get_n(2)?,
            ..Default::default()
        };

        if element.global_trim_mode == 2 {
            for trim in &mut element.trims {
                let mut t = Trim {
                    b_default_trim: reader.get()?,
                    ..Default::default()
                };

                if t.b_default_trim {
                    // Renderer derives this trim itself. Distinct from "disabled" and
                    // from "element absent", so record it rather than leaving it None.
                    *trim = Some(t);
                    continue;
                }

                t.b_disable_trim = reader.get()?;
                if !t.b_disable_trim {
                    let trim_balance_presence = reader.get_n::<u8>(5)?;

                    if trim_balance_presence & 0x1 != 0 {
                        t.trim_centre = Some(TRIM_LUT[reader.get_n::<u8>(4)? as usize]);
                    }

                    if trim_balance_presence & 0x2 != 0 {
                        t.trim_surround = TRIM_ATTENUATION_LUT[reader.get_n::<u8>(4)? as usize];
                    }

                    if trim_balance_presence & 0x4 != 0 {
                        t.trim_height = TRIM_ATTENUATION_LUT[reader.get_n::<u8>(4)? as usize];
                    }

                    if trim_balance_presence & 0x8 != 0 {
                        let sign = if reader.get()? { 1.0 } else { -1.0 };
                        t.bal3d_y_tb = Some((reader.get_n::<u8>(4)? + 1) as f64 / 16.0 * sign);
                    }

                    if trim_balance_presence & 0x10 != 0 {
                        let sign = if reader.get()? { 1.0 } else { -1.0 };
                        t.bal3d_y_lis = Some((reader.get_n::<u8>(4)? + 1) as f64 / 16.0 * sign);
                    }
                }

                *trim = Some(t)
            }
        }

        element.b_disable_trim_per_obj = reader.get()?;
        if element.b_disable_trim_per_obj {
            for _ in 0..state.object_count {
                element.b_disable_trim.push(reader.get()?);
            }
        }

        Ok(element)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ExtendedObjectElement {
    pub b_obj_div_block: bool,
    pub object_div_block: Vec<Vec<ObjectDivergenceBlock>>,

    pub b_ext_prec_pos_block: bool,
    pub ext_prec_pos_block: Vec<Vec<ExtendedPrecisionPositionBlock>>,
}

impl ExtendedObjectElement {
    fn read(state: &mut OAMDParserState, reader: &mut SliceReader) -> Result<Self> {
        let mut element = Self {
            b_obj_div_block: reader.get()?,
            ..Default::default()
        };

        let object_count = state.object_count;
        let Some(object_element) = &state.object_element else {
            return Ok(element);
        };

        let block_count = object_element.md_update_info.num_obj_info_blocks;

        if element.b_obj_div_block {
            let mut obj_blk = Vec::with_capacity(object_count);

            for object_index in 0..object_count {
                let mut blk_blk: Vec<ObjectDivergenceBlock> = Vec::with_capacity(block_count);

                let object_data = &object_element.object_data[object_index];
                for (block_index, object_info_block) in object_data.iter().enumerate() {
                    let mut blk = ObjectDivergenceBlock::default();

                    if !object_info_block.b_object_not_active
                        && !object_info_block.b_object_in_bed_or_isf
                    {
                        blk.b_object_divergence = reader.get()?;

                        if blk.b_object_divergence {
                            let object_div_mode = reader.get_n::<u8>(2)?;
                            match object_div_mode {
                                0 => {
                                    blk.object_divergence =
                                        // object_div_table
                                        ObjectDivergenceBlock::OBJECT_DIV_TABLE_TABLE
                                            [reader.get_n::<u8>(2)? as usize];
                                }
                                // Reuse the previous block's value. At block 0 that block is
                                // the previous payload's last, which only the caller has, so the
                                // value here is 0 there and in every block that reuses it in
                                // turn: a caller that keeps the last value it applied resolves
                                // object_div_mode 1 from that, at every block.
                                1 => {
                                    if let Some(prev_blk) = blk_blk.last() {
                                        blk.object_divergence = prev_blk.object_divergence;
                                    }
                                }
                                _ => {
                                    blk.object_div_code = reader.get_n(6)?;
                                    if let Some(div) = ObjectDivergenceBlock::OBJECT_DIV_CODE_TABLE
                                        [blk.object_div_code as usize]
                                    {
                                        blk.object_divergence = div;
                                    } else {
                                        warn!(
                                            "Invalid object_div_code for object {object_index} block {block_index}"
                                        )
                                    }
                                }
                            }
                            blk.b_object_divergence = true;
                            blk.object_div_mode = object_div_mode;
                        }
                    }

                    blk_blk.push(blk);
                }

                obj_blk.push(blk_blk);
            }

            element.object_div_block = obj_blk;
        }

        element.b_ext_prec_pos_block = reader.get()?;

        if element.b_ext_prec_pos_block {
            let mut pos_blk = Vec::with_capacity(object_count);

            for object_index in 0..object_count {
                let mut blk_blk = Vec::with_capacity(block_count);

                let object_data = &object_element.object_data[object_index]; // has length == block_count
                for object_info_block in object_data.iter() {
                    let mut blk = ExtendedPrecisionPositionBlock::default();

                    if !object_info_block.b_object_not_active {
                        // b_ext_prec_pos
                        if !object_info_block.b_object_in_bed_or_isf && reader.get()? {
                            let ext_prec_pos_presence = reader.get_n::<u8>(3)?;

                            if ext_prec_pos_presence & 1 != 0 {
                                blk.ext_prec_pos3d_x =
                                    ExtendedPrecisionPositionBlock::EXT_PREC_POS3D_LUT
                                        [reader.get_n::<u8>(2)? as usize]
                                        / 310.0;
                            }

                            if ext_prec_pos_presence & 2 != 0 {
                                blk.ext_prec_pos3d_y =
                                    ExtendedPrecisionPositionBlock::EXT_PREC_POS3D_LUT
                                        [reader.get_n::<u8>(2)? as usize]
                                        / 310.0;
                            }

                            if ext_prec_pos_presence & 4 != 0 {
                                blk.ext_prec_pos3d_z =
                                    ExtendedPrecisionPositionBlock::EXT_PREC_POS3D_LUT
                                        [reader.get_n::<u8>(2)? as usize]
                                        / 75.0;
                            }
                        }
                    }

                    blk_blk.push(blk);
                }

                pos_blk.push(blk_blk);
            }

            element.ext_prec_pos_block = pos_blk;
        }

        Ok(element)
    }
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ObjectDivergenceBlock {
    pub object_divergence: f64,
    pub b_object_divergence: bool,
    pub object_div_mode: u8,
    pub object_div_table: u8,
    pub object_div_code: u8,
}

impl ObjectDivergenceBlock {
    pub const OBJECT_DIV_TABLE_TABLE: [f64; 4] = [0.500755, 0.608529, 0.704833, 1.0]; // 26, 29, 32, 63

    pub const OBJECT_DIV_CODE_TABLE: [Option<f64>; 64] = [
        Some(0.0),      // 0
        Some(0.0),      // 1
        Some(0.004026), // 2
        Some(0.00716),  // 3
        Some(0.012731), // 4
        Some(0.020173), // 5
        Some(0.028485), // 6
        Some(0.04021),  // 7
        Some(0.050582), // 8
        Some(0.063601), // 9
        Some(0.079914), // 10
        Some(0.100299), // 11
        Some(0.125666), // 12
        Some(0.140532), // 13
        Some(0.157027), // 14
        Some(0.175282), // 15
        Some(0.195417), // 16
        Some(0.217536), // 17
        Some(0.241718), // 18
        Some(0.268002), // 19
        Some(0.296377), // 20
        Some(0.326766), // 21
        Some(0.359017), // 22
        Some(0.392895), // 23
        Some(0.428081), // 24
        Some(0.464184), // 25
        Some(0.500755), // 26
        Some(0.537316), // 27
        Some(0.573389), // 28
        Some(0.608529), // 29
        Some(0.642346), // 30
        Some(0.674524), // 31
        Some(0.704833), // 32
        Some(0.733123), // 33
        Some(0.75932),  // 34
        Some(0.783416), // 35
        Some(0.805451), // 36
        Some(0.825506), // 37
        Some(0.843686), // 38
        Some(0.860112), // 39
        Some(0.874914), // 40
        Some(0.888222), // 41
        Some(0.900168), // 42
        Some(0.910875), // 43
        Some(0.920461), // 44
        Some(0.929035), // 45
        Some(0.936698), // 46
        Some(0.943544), // 47
        Some(0.949656), // 48
        Some(0.955112), // 49
        Some(0.95998),  // 50
        Some(0.964322), // 51
        Some(0.968195), // 52
        Some(0.974729), // 53
        Some(0.979923), // 54
        Some(0.98405),  // 55
        Some(0.98733),  // 56
        Some(0.989935), // 57
        Some(0.992874), // 58
        Some(0.994955), // 59
        Some(0.996817), // 60
        Some(0.99821),  // 61
        Some(0.998993), // 62
        Some(1.0),      // 63
    ];
}

#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct ExtendedPrecisionPositionBlock {
    pub ext_prec_pos3d_x: f64,
    pub ext_prec_pos3d_y: f64,
    pub ext_prec_pos3d_z: f64,
}

impl ExtendedPrecisionPositionBlock {
    pub const EXT_PREC_POS3D_LUT: [f64; 4] = [1.0, 2.0, -1.0, -2.0];
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SpeakerLabels {
    L,
    R,
    C,
    LFE,
    Lss,
    Rss,
    Lrs,
    Rrs,
    Lfh,
    Rfh,
    Lts,
    Rts,
    Lrh,
    Rrh,
    Lw,
    Rw,
    LFE2,
}

impl SpeakerLabels {
    const W: f32 = 42.0 / 62.0;

    #[rustfmt::skip]
    const POSITIONS: [[f32; 3]; 17] = [
        [-1.0,      1.0,      0.0],
        [ 1.0,      1.0,      0.0],
        [ 0.0,      1.0,      0.0],
        [-1.0,      1.0,     -1.0],
        [-1.0,      0.0,      0.0],
        [ 1.0,      0.0,      0.0],
        [-1.0,     -1.0,      0.0],
        [ 1.0,     -1.0,      0.0],
        [-1.0,      1.0,      1.0],
        [ 1.0,      1.0,      1.0],
        [-1.0,      0.0,      1.0],
        [ 1.0,      0.0,      1.0],
        [-1.0,     -1.0,      1.0],
        [ 1.0,     -1.0,      1.0],
        [-1.0,  Self::W,      0.0],
        [ 1.0,  Self::W,      0.0],
        [ 1.0,      1.0,     -1.0],
    ];

    /// In the order the label codes are assigned, so the index is the code.
    pub const ALL: [Self; 17] = [
        Self::L,
        Self::R,
        Self::C,
        Self::LFE,
        Self::Lss,
        Self::Rss,
        Self::Lrs,
        Self::Rrs,
        Self::Lfh,
        Self::Rfh,
        Self::Lts,
        Self::Rts,
        Self::Lrh,
        Self::Rrh,
        Self::Lw,
        Self::Rw,
        Self::LFE2,
    ];

    pub fn from_u8(n: u8) -> Option<Self> {
        Self::ALL.get(n as usize).copied()
    }

    pub fn pos(&self) -> &[f32; 3] {
        &Self::POSITIONS[*self as usize]
    }
}

/// `STD_BED_LIST` is a bit-indexed partition of the speaker labels: bit *n* of a standard bed
/// assignment turns on group *n*. It must cover every label exactly once, because a gap would
/// silently drop a speaker from a bed and an overlap would let two bits fight over one.
#[test]
fn std_bed_list_partitions_the_speaker_labels() {
    let mut seen = vec![0usize; SpeakerLabels::LFE2 as usize + 1];
    for group in STD_BED_LIST {
        for &n in group {
            seen[n] += 1;
        }
    }
    assert!(
        seen.iter().all(|&c| c == 1),
        "each label must appear in exactly one group, got {seen:?}"
    );

    // the groups are pairs in label order, with the three mono speakers alone
    assert_eq!(
        STD_BED_LIST[0],
        &[SpeakerLabels::L as usize, SpeakerLabels::R as usize]
    );
    assert_eq!(STD_BED_LIST[1], &[SpeakerLabels::C as usize]);
    assert_eq!(STD_BED_LIST[2], &[SpeakerLabels::LFE as usize]);
    assert_eq!(STD_BED_LIST[9], &[SpeakerLabels::LFE2 as usize]);
}

/// The two ways to build an LFE-only bed must agree. `from_std` walks `STD_BED_LIST` while
/// `with_lfe_only` sets the label directly, so a reordering of the table would make them
/// disagree with nothing else to notice. An LFE-only bed is the common Atmos shape, not a
/// corner case.
#[test]
fn lfe_only_bed_agrees_between_both_constructors() {
    let from_bit = BedAssignment::from_std(1 << 2);
    let direct = BedAssignment::with_lfe_only();
    assert_eq!(
        from_bit.0, direct.0,
        "bit 2 of a standard bed assignment is the LFE group"
    );
    assert_eq!(direct.0.iter().filter(|&&b| b).count(), 1);
}

/// Distances are a fixed ladder resolved by a 4-bit index, so an out-of-order or truncated
/// table would place objects at the wrong distance with no error anywhere.
#[test]
fn distance_factors_are_a_rising_ladder_of_sixteen() {
    assert_eq!(DISTANCE_FACTORS.len(), 16);
    assert!(
        DISTANCE_FACTORS.windows(2).all(|w| w[0] < w[1]),
        "must be strictly increasing: {DISTANCE_FACTORS:?}"
    );
    assert_eq!(DISTANCE_FACTORS[0], 1.1);
    assert_eq!(DISTANCE_FACTORS[15], 50.1);
}

/// The ISF table is indexed by a 3-bit field, so it needs eight entries even though only six
/// are defined. Indexing a six-entry table panicked on the reserved values until 0.6.4.
#[test]
fn isf_count_list_covers_the_full_three_bit_index() {
    assert_eq!(ISF_COUNT_LIST.len(), 8);
    assert_eq!(&ISF_COUNT_LIST[..6], &[4, 8, 10, 14, 15, 30]);
    assert_eq!(
        &ISF_COUNT_LIST[6..],
        &[0, 0],
        "reserved indices carry no objects"
    );
    assert!(ISF_COUNT_LIST[..6].windows(2).all(|w| w[0] < w[1]));
}

/// The extended-precision position offsets are a 2-bit signed ladder applied on top of a
/// coarse position, so sign order matters: swapping the halves would mirror every refinement.
#[test]
fn extended_precision_offsets_pair_positive_then_negative() {
    assert_eq!(
        ExtendedPrecisionPositionBlock::EXT_PREC_POS3D_LUT,
        [1.0, 2.0, -1.0, -2.0]
    );
}

/// `POSITIONS` is already in DAMF's coordinate space, not an internal one: x runs -1 left to
/// +1 right, y **+1 front to -1 back**, and z 0 at ear level to 1 at the ceiling. Getting the
/// y sign backwards puts every object on the wrong axis, and nothing else in the crate would
/// complain. Confirmed against a master whose bed objects carry `[-1,1,0]`,
/// `[0,1,0]` and `[-1,0,0]` for L, C and Lss.
#[test]
fn speaker_positions_are_damf_coordinates() {
    use SpeakerLabels::*;

    // fronts sit at y = +1, rears at y = -1, sides at 0
    assert_eq!(L.pos(), &[-1.0, 1.0, 0.0]);
    assert_eq!(R.pos(), &[1.0, 1.0, 0.0]);
    assert_eq!(C.pos(), &[0.0, 1.0, 0.0]);
    assert_eq!(Lss.pos(), &[-1.0, 0.0, 0.0]);
    assert_eq!(Rss.pos(), &[1.0, 0.0, 0.0]);
    assert_eq!(Lrs.pos(), &[-1.0, -1.0, 0.0]);
    assert_eq!(Rrs.pos(), &[1.0, -1.0, 0.0]);

    // heights repeat the floor plan at z = 1
    assert_eq!(Lfh.pos(), &[-1.0, 1.0, 1.0]);
    assert_eq!(Rfh.pos(), &[1.0, 1.0, 1.0]);
    assert_eq!(Lts.pos(), &[-1.0, 0.0, 1.0]);
    assert_eq!(Rts.pos(), &[1.0, 0.0, 1.0]);
    assert_eq!(Lrh.pos(), &[-1.0, -1.0, 1.0]);
    assert_eq!(Rrh.pos(), &[1.0, -1.0, 1.0]);

    // the wides are the only off-axis pair, at 42/62 on the 1/62 position grid
    assert_eq!(Lw.pos(), &[-1.0, 42.0 / 62.0, 0.0]);
    assert_eq!(Rw.pos(), &[1.0, 42.0 / 62.0, 0.0]);

    // the LFEs are the only entries below ear level
    assert_eq!(LFE.pos(), &[-1.0, 1.0, -1.0]);
    assert_eq!(LFE2.pos(), &[1.0, 1.0, -1.0]);
}

/// Every label must resolve, and the discriminants must stay dense: `from_u8` gates on
/// `<= LFE2` and `pos()` indexes `POSITIONS` directly, so a gap or a reordering would index
/// the wrong row rather than fail.
#[test]
fn every_speaker_label_round_trips_to_a_position() {
    for n in 0..=SpeakerLabels::LFE2 as u8 {
        let label = SpeakerLabels::from_u8(n).unwrap_or_else(|| panic!("label {n} must map"));
        assert_eq!(label as u8, n, "discriminant {n} must round trip");
        let p = label.pos();
        assert!(
            (-1.0..=1.0).contains(&p[0]) && (-1.0..=1.0).contains(&p[1]),
            "{label:?} position {p:?} outside the DAMF range"
        );
    }
    assert!(SpeakerLabels::from_u8(SpeakerLabels::LFE2 as u8 + 1).is_none());
}

/// A small OAMD payload, for documentation examples and for a consumer's own tests.
pub const EXAMPLE_DATA: &[u8] = &[
    0x1F, 0x88, 0x4B, 0x80, 0x00, 0xA2, 0x70, 0x00, 0x80, 0x40, 0xE0, 0x01, 0x00, 0x81, 0xC0, 0x02,
    0x01, 0x03, 0x80, 0x04, 0x02, 0x07, 0x00, 0x08, 0x04, 0x0E, 0x00, 0x10, 0x08, 0x1C, 0x00, 0x20,
    0x10, 0x38, 0x00, 0x40, 0x20, 0x70, 0x00, 0x80, 0x40, 0xE0, 0x01, 0x00, 0x81, 0xC0, 0x02, 0x01,
    0x03, 0x80, 0x04, 0x02, 0x07, 0x00, 0x08, 0x04, 0x0E, 0x00, 0x10, 0x08, 0x1C, 0x00, 0x20, 0x10,
    0x02, 0x40, 0x24, 0x33, 0x33, 0xF8, 0x00,
];

#[cfg(test)]
const TEST_DATA_TRIM: &[u8] = &[
    0x1F, 0x88, 0x4B, 0x80, 0x00, 0xA2, 0x70, 0x00, 0x80, 0x40, 0xE4, 0x0B, 0x40, 0x81, 0xDF, 0x02,
    0x01, 0x03, 0x80, 0xFC, 0x02, 0x07, 0xD4, 0x5A, 0x04, 0x0F, 0xF0, 0x10, 0x08, 0x1C, 0x0F, 0xA0,
    0x10, 0x38, 0x00, 0x7C, 0x20, 0x7F, 0x9F, 0x80, 0x40, 0xFF, 0x7D, 0x00, 0x81, 0xFE, 0x03, 0xE1,
    0x03, 0x81, 0xF7, 0xC2, 0x07, 0xFB, 0xEF, 0x84, 0x0E, 0x00, 0x10, 0x08, 0x1C, 0x00, 0x20, 0x10,
    0x02, 0xB2, 0x20, 0xCC, 0xE6, 0xAB, 0xEF, 0x0C, 0xED, 0x0D, 0x29, 0x86, 0x85, 0x80,
];

#[cfg(test)]
const TEST_DATA_BED_OBJECT: &[u8] = &[
    0x12, 0x11, 0x2b, 0xc4, 0x30, 0, 0x0f, 0xf1, 0x08, 0x42, 0x10, 0x84, 0x21, 0x08, 0x62, 0x22,
    0x8b, 0x32, 0,
];

#[cfg(test)]
const TEST_DATA_BROKEN: &[u8] = &[
    0x1F, 0x88, 0x4B, 0x80, 0x00, 0xA2, 0x70, 0x00, 0x80, 0x40, 0xE4, 0x0B, 0x40, 0x81, 0xDF, 0x02,
    0x01, 0x03, 0x80, 0xFC, 0x02, 0x07, 0xD4, 0x5A, 0x04, 0x0F, 0xF0, 0x10, 0x08, 0x1C, 0x0F, 0xA0,
    0x10, 0x38, 0x00, 0x7C, 0x20, 0x7F, 0x9F, 0x80, 0x40, 0xFF, 0x7D, 0x00, 0x81, 0xFE, 0x03, 0xE1,
    0x03, 0x81, 0xF7, 0xC2, 0x07, 0xFB, 0xEF, 0x84, 0x0E, 0x00, 0x10, 0x08, 0x1C, 0x00, 0x20, 0x10,
    0x02, 0x30, 0x20, 0xCC, 0xFF, 0xE0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bed_object() {
        let payload = ObjectAudioMetadataPayload::read(TEST_DATA_BED_OBJECT).unwrap();
        let parsed = payload.bed_object_element.as_ref().unwrap();
        assert_eq!(parsed.presence, [true, true, false, false, false, true]);
        assert_eq!(parsed.top4_to_top2, Some(1));
        assert_eq!(
            parsed.to_5_1[1],
            Some(DownmixRoute {
                destination: 1,
                gain_code: 1
            })
        );
        assert_eq!(
            parsed.to_5_1[2],
            Some(DownmixRoute {
                destination: 0,
                gain_code: 1
            })
        );

        // Read from a reader that ends short of the element, it fails rather than reading on.
        for bytes in 1..3 {
            let mut reader = SliceReader::new(TEST_DATA_BED_OBJECT);
            reader.skip_n(122).unwrap();
            let body = reader.take(8 * bytes).unwrap();
            assert!(
                BedObjectElement::read(&payload.program_assignment, &mut SliceReader::new(&body))
                    .is_err()
            );
        }

        let mut reader = SliceReader::new(TEST_DATA_BED_OBJECT);
        reader.skip_n(122).unwrap();
        assert_eq!(
            &BedObjectElement::read(&payload.program_assignment, &mut reader).unwrap(),
            parsed
        );
        assert_eq!(reader.position().unwrap(), 143);
    }

    /// A truncated element is not fatal: it is either dropped or, when only its padding was cut
    /// off, read in full.
    #[test]
    fn bed_object_truncated() {
        let full = ObjectAudioMetadataPayload::read(TEST_DATA_BED_OBJECT)
            .unwrap()
            .bed_object_element;
        for len in 16..TEST_DATA_BED_OBJECT.len() {
            let payload = ObjectAudioMetadataPayload::read(&TEST_DATA_BED_OBJECT[..len])
                .unwrap_or_else(|e| panic!("length {len}: {e}"));
            assert!(
                payload.bed_object_element.is_none() || payload.bed_object_element == full,
                "length {len}"
            );
        }
    }
    use anyhow::Result;

    #[test]
    fn test1() -> Result<()> {
        let _ = ObjectAudioMetadataPayload::read(EXAMPLE_DATA)?;
        Ok(())
    }

    #[test]
    fn trim() -> Result<()> {
        let payload = ObjectAudioMetadataPayload::read(TEST_DATA_TRIM)?;
        // trim_balance_presence[] in reading order, [0] the first bit (table 34: [4] centre, [0]
        // listener-plane balance). Read the other way round it ends 43 bits short of its size,
        // with 17 of those bits set where only zero padding may be.
        let trim = payload.trim_element.expect("a trim element");
        assert_eq!(trim.global_trim_mode, 2);
        let t = |i: usize| trim.trims[i].clone().unwrap();
        assert_eq!(
            (t(0).trim_centre, t(0).trim_surround, t(0).trim_height),
            (None, Some(-3.0), Some(-4.5))
        );
        assert_eq!(
            (t(1).trim_surround, t(1).bal3d_y_tb, t(1).bal3d_y_lis),
            (Some(-9.0), Some(1.0), Some(-1.0))
        );

        Ok(())
    }

    #[test]
    fn broken() -> Result<()> {
        let _ = ObjectAudioMetadataPayload::read(TEST_DATA_BROKEN)?;

        Ok(())
    }

    /// Bits, most significant first, so a payload can be written the way it is read.
    #[derive(Default)]
    struct Bits {
        data: Vec<u8>,
        len: usize,
    }

    impl Bits {
        fn push(&mut self, n: usize, value: u32) {
            for i in (0..n).rev() {
                if self.len.is_multiple_of(8) {
                    self.data.push(0);
                }
                if (value >> i) & 1 == 1 {
                    let last = self.data.len() - 1;
                    self.data[last] |= 1 << (7 - (self.len & 7));
                }
                self.len += 1;
            }
        }
    }

    /// A payload carrying one element and nothing else: one dynamic object, no bed.
    /// `body` writes the element's own bits, after the id, size and discard flag.
    fn payload_with_element(id: u32, body: impl Fn(&mut Bits)) -> Vec<u8> {
        payload_with_objects(1, id, body)
    }

    /// `payload_with_element` with `objects` dynamic objects.
    fn payload_with_objects(objects: u32, id: u32, body: impl Fn(&mut Bits)) -> Vec<u8> {
        let mut element = Bits::default();
        body(&mut element);

        let mut bits = Bits::default();
        bits.push(2, 0); // oamd_version
        bits.push(5, objects - 1); // object_count - 1
        bits.push(1, 1); // b_dyn_object_only_program
        bits.push(1, 0); // b_lfe_present
        bits.push(1, 0); // b_alternate_object_data_present
        bits.push(4, 1); // oa_element_count
        bits.push(4, id); // oa_element_id_idx

        // oa_element_size is in bytes minus one, and must cover the element's own bits
        // plus the discard flag that precedes them. It is a variable-bits field, so the
        // four bits are followed by the bit that says whether another group follows.
        let bytes = (element.len + 1).div_ceil(8) as u32;
        bits.push(4, bytes - 1);
        bits.push(1, 0); // no further size group
        bits.push(1, 0); // b_discard_unknown_element

        for i in 0..element.len {
            bits.push(1, ((element.data[i / 8] >> (7 - (i % 8))) & 1) as u32);
        }

        bits.data
    }

    /// Mode 0 states one intent for everything and reads nothing further, so a payload
    /// carrying only the two header fields is complete.
    #[test]
    fn headphone_element_mode_0_applies_to_every_object() -> Result<()> {
        let data = payload_with_element(3, |b| {
            b.push(3, 0); // hp_mode
            b.push(3, 0); // num_obj_info_blocks - 1
        });

        let payload = ObjectAudioMetadataPayload::read(&data)?;
        let element = payload.headphone_element.expect("a headphone element");

        assert_eq!(element.hp_mode, 0);
        assert_eq!(element.num_obj_info_blocks, 1);
        assert_eq!(element.headphone_data.len(), 1, "one object");
        assert_eq!(element.headphone_data[0][0].hp_render_mode, 0);
        assert!(element.headphone_data[0][0].hp_head_track_disable);

        Ok(())
    }

    /// Mode 1 reads one head-tracking bit and applies it everywhere.
    #[test]
    fn headphone_element_mode_1_reads_one_bit() -> Result<()> {
        let data = payload_with_element(3, |b| {
            b.push(3, 1); // hp_mode
            b.push(3, 1); // two blocks
            b.push(1, 0); // hp_head_track_disable
        });

        let element = ObjectAudioMetadataPayload::read(&data)?
            .headphone_element
            .expect("a headphone element");

        assert_eq!(element.num_obj_info_blocks, 2);
        assert_eq!(element.headphone_data[0].len(), 2);
        for block in &element.headphone_data[0] {
            assert_eq!(block.hp_render_mode, 1);
            assert!(!block.hp_head_track_disable);
        }

        Ok(())
    }

    /// Mode 3 codes each object and block: two bits of render mode, then the bit.
    #[test]
    fn headphone_element_mode_3_codes_each_block() -> Result<()> {
        let data = payload_with_element(3, |b| {
            b.push(3, 3); // hp_mode
            b.push(3, 1); // two blocks
            b.push(2, 2); // block 0 render mode
            b.push(1, 1); // block 0 head track disable
            b.push(2, 3); // block 1 render mode: mid
            b.push(1, 0);
        });

        let element = ObjectAudioMetadataPayload::read(&data)?
            .headphone_element
            .expect("a headphone element");

        assert_eq!(element.headphone_data[0][0].hp_render_mode, 2);
        assert!(element.headphone_data[0][0].hp_head_track_disable);
        assert_eq!(element.headphone_data[0][1].hp_render_mode, 3);
        assert!(!element.headphone_data[0][1].hp_head_track_disable);

        Ok(())
    }

    /// A mode the element does not define reads nothing further and states nothing.
    #[test]
    fn headphone_element_leaves_an_undefined_mode_empty() -> Result<()> {
        let data = payload_with_element(3, |b| {
            b.push(3, 7);
            b.push(3, 0);
        });

        let element = ObjectAudioMetadataPayload::read(&data)?
            .headphone_element
            .expect("a headphone element");

        assert_eq!(element.hp_mode, 7);
        assert!(element.headphone_data.is_empty());

        Ok(())
    }

    /// Sub-mode 0 codes one bit per object and block, and the bit is not the indication.
    #[test]
    fn object_description_sub_mode_0_maps_its_bit() -> Result<()> {
        let data = payload_with_element(4, |b| {
            b.push(3, 0); // num_obj_info_blocks - 1
            b.push(1, 1); // b_present
            b.push(2, 0); // sub_mode
            b.push(1, 1); // the object's only block
        });

        let element = ObjectAudioMetadataPayload::read(&data)?
            .object_description_element
            .expect("an object description element");

        assert!(element.b_present);
        assert_eq!(element.object_dialog_indication, vec![vec![1]]);

        Ok(())
    }

    /// Sub-mode 1 codes two bits and maps them, and code 2 carries five further bits
    /// when the element says so, which must be consumed or the next object shifts.
    #[test]
    fn object_description_sub_mode_1_maps_and_consumes_the_extra() -> Result<()> {
        let data = payload_with_element(4, |b| {
            b.push(3, 1); // two blocks
            b.push(1, 1); // b_present
            b.push(2, 1); // sub_mode
            b.push(1, 1); // b_extra
            b.push(2, 2); // block 0: the code that carries more
            b.push(5, 0x15); // and the five bits it carries
            b.push(2, 3); // block 1
        });

        let element = ObjectAudioMetadataPayload::read(&data)?
            .object_description_element
            .expect("an object description element");

        assert_eq!(element.object_dialog_indication, vec![vec![3, 0]]);

        Ok(())
    }

    /// A payload with no element at all still parses, and states nothing.
    #[test]
    fn object_description_absent_when_not_present() -> Result<()> {
        let data = payload_with_element(4, |b| {
            b.push(3, 0);
            b.push(1, 0); // b_present
        });

        let element = ObjectAudioMetadataPayload::read(&data)?
            .object_description_element
            .expect("an object description element");

        assert!(!element.b_present);
        assert!(element.object_dialog_indication.is_empty());

        Ok(())
    }

    /// md_update_info with one block at offset 0 and no ramp, and the element's reserved flag.
    fn object_element_header(b: &mut Bits, sample_offset_code: u32, blocks: u32) {
        b.push(2, sample_offset_code);
        b.push(3, blocks - 1);
        for _ in 0..blocks {
            b.push(6, 0); // block_offset_factor_bits
            b.push(2, 0); // ramp_duration_code
        }
        b.push(1, 1); // b_reserved_data_not_present
    }

    /// Block 0 of an active object: gain as coded, default priority, position (0, 0, +0), the
    /// zone code, size 0, screen reference with `depth_idx`, no snap, no additional data.
    fn first_block(b: &mut Bits, gain_code: u32, gain_bits: u32, zone: u32, depth_idx: u32) {
        b.push(1, 0); // b_object_not_active
        b.push(2, gain_code);
        if gain_code == 2 {
            b.push(6, gain_bits);
        }
        b.push(1, 1); // b_default_object_priority
        b.push(6, 0);
        b.push(6, 0);
        b.push(1, 1); // pos3D_Z_sign_bits
        b.push(4, 0);
        b.push(1, 0); // b_object_distance_specified
        b.push(3, zone);
        b.push(1, 1); // b_enable_elevation
        b.push(2, 0); // object_size_idx
        b.push(1, 1); // b_object_use_screen_ref
        b.push(3, 7); // screen_factor_bits
        b.push(2, depth_idx);
        b.push(1, 0); // b_object_snap
        b.push(1, 0); // b_additional_table_data_exists
    }

    /// A version this parser cannot read is skipped, not an error that would fail its access unit.
    #[test]
    fn other_versions_are_skipped() -> Result<()> {
        let mut bits = Bits::default();
        bits.push(2, 1); // oamd_version 1
        bits.push(6, 0);
        let payload = ObjectAudioMetadataPayload::read(&bits.data)?;
        assert_eq!(payload.oamd_version, 1);
        assert!(payload.object_element.is_none() && payload.oa_element_md.is_empty());
        Ok(())
    }

    /// A second element of a type already read is skipped by its size; the first stands.
    #[test]
    fn duplicate_elements_keep_the_first() -> Result<()> {
        let element = |depth_idx: u32| {
            let mut b = Bits::default();
            object_element_header(&mut b, 0, 1);
            first_block(&mut b, 0, 0, 0, depth_idx);
            b
        };
        let (first, second) = (element(3), element(0));
        let mut bits = Bits::default();
        bits.push(2, 0); // oamd_version
        bits.push(5, 0); // object_count - 1
        bits.push(1, 1); // b_dyn_object_only_program
        bits.push(1, 0); // b_lfe_present
        bits.push(1, 0); // b_alternate_object_data_present
        bits.push(4, 2); // oa_element_count
        for body in [&first, &second] {
            let bytes = (body.len + 1).div_ceil(8);
            bits.push(4, 1); // oa_element_id_idx: object element
            bits.push(4, (bytes - 1) as u32);
            bits.push(1, 0);
            let start = bits.len;
            bits.push(1, 0); // b_discard_unknown_element
            for i in 0..body.len {
                bits.push(1, ((body.data[i / 8] >> (7 - (i % 8))) & 1) as u32);
            }
            while bits.len < start + 8 * bytes {
                bits.push(1, 0); // padding
            }
        }
        let payload = ObjectAudioMetadataPayload::read(&bits.data)?;
        assert_eq!(payload.oa_element_md.len(), 2);
        let first = payload.object_element.unwrap();
        assert_eq!(first.object_data[0][0].object_render_info.depth_factor, 2.0);
        Ok(())
    }

    /// An element whose fields run past its declared size is dropped, and the element after it is
    /// read from where that size ends.
    #[test]
    fn elements_end_at_their_declared_size() -> Result<()> {
        let mut object = Bits::default();
        object_element_header(&mut object, 0, 1);
        first_block(&mut object, 0, 0, 0, 3);
        let mut trim = Bits::default();
        trim.push(2, 1); // warp_mode
        trim.push(2, 0);
        trim.push(2, 1); // global_trim_mode: disable
        trim.push(1, 0); // b_disable_trim_per_obj
        let mut bits = Bits::default();
        bits.push(2, 0); // oamd_version
        bits.push(5, 0); // object_count - 1
        bits.push(1, 1); // b_dyn_object_only_program
        bits.push(1, 0); // b_lfe_present
        bits.push(1, 0); // b_alternate_object_data_present
        bits.push(4, 2); // oa_element_count
        // The object element declares a byte less than its fields take.
        for (id, body, short) in [(1, &object, 1), (2, &trim, 0)] {
            let bytes = (body.len + 1).div_ceil(8) - short;
            bits.push(4, id);
            bits.push(4, (bytes - 1) as u32);
            bits.push(1, 0);
            let start = bits.len;
            bits.push(1, 0); // b_discard_unknown_element
            for i in 0..body.len.min(8 * bytes - 1) {
                bits.push(1, ((body.data[i / 8] >> (7 - (i % 8))) & 1) as u32);
            }
            while bits.len < start + 8 * bytes {
                bits.push(1, 0); // padding
            }
        }
        let payload = ObjectAudioMetadataPayload::read(&bits.data)?;
        assert_eq!(payload.oa_element_md.len(), 2);
        assert!(payload.object_element.is_none());
        let trim = payload.trim_element.expect("the element after the bad one");
        assert_eq!((trim.warp_mode, trim.global_trim_mode), (1, 1));
        Ok(())
    }

    /// The last element is read up to the end of the payload, whatever size it declares: nothing
    /// after it depends on where that size ends.
    #[test]
    fn last_element_reads_past_a_short_size() -> Result<()> {
        let mut object = Bits::default();
        object_element_header(&mut object, 0, 1);
        first_block(&mut object, 2, 9, 0, 3);
        let mut bits = Bits::default();
        bits.push(2, 0); // oamd_version
        bits.push(5, 0); // object_count - 1
        bits.push(1, 1); // b_dyn_object_only_program
        bits.push(1, 0); // b_lfe_present
        bits.push(1, 0); // b_alternate_object_data_present
        bits.push(4, 1); // oa_element_count
        // The element declares a byte less than its fields take, and all of them follow.
        let bytes = (object.len + 1).div_ceil(8) - 1;
        bits.push(4, 1);
        bits.push(4, (bytes - 1) as u32);
        bits.push(1, 0);
        bits.push(1, 0); // b_discard_unknown_element
        for i in 0..object.len {
            bits.push(1, ((object.data[i / 8] >> (7 - (i % 8))) & 1) as u32);
        }
        let payload = ObjectAudioMetadataPayload::read(&bits.data)?;
        let block = &payload
            .object_element
            .expect("the element, read in full")
            .object_data[0][0];
        assert_eq!(block.object_basic_info.object_gain, 6);
        assert_eq!(block.object_render_info.depth_factor, 2.0);
        Ok(())
    }

    /// Table 16 depth factors, table 20's reserved zone 6, table 22's reserved sample offset code.
    #[test]
    fn render_info_tables() -> Result<()> {
        for (depth_idx, depth) in [(0, 0.25), (1, 0.5), (2, 1.0), (3, 2.0)] {
            let data = payload_with_element(1, |b| {
                object_element_header(b, 3, 1);
                first_block(b, 0, 0, 6, depth_idx);
            });
            let element = ObjectAudioMetadataPayload::read(&data)?
                .object_element
                .unwrap();
            assert_eq!(element.md_update_info.sample_offset, 0);
            let render = &element.object_data[0][0].object_render_info;
            assert_eq!(render.depth_factor, depth);
            assert_eq!(render.screen_factor, 1.0);
            assert_eq!(render.zone_constraints_idx, 0);
        }
        Ok(())
    }

    /// object_gain_idx 3 takes the previous object's gain in the block however that object got
    /// it, here by reusing its own from the block before (status 2).
    #[test]
    fn gain_of_previous_object() -> Result<()> {
        let data = payload_with_objects(2, 1, |b| {
            object_element_header(b, 0, 2);
            first_block(b, 2, 9, 0, 0); // object 0, block 0: +6 dB
            b.push(1, 0); // object 0, block 1: active
            b.push(2, 2); // basic info: as block 0
            b.push(2, 2); // render info: as block 0
            b.push(1, 0); // b_additional_table_data_exists
            first_block(b, 3, 0, 0, 0); // object 1, block 0: object 0's gain
            b.push(1, 0); // object 1, block 1: active
            b.push(2, 1); // basic info: all of it
            b.push(2, 3); // object_gain_idx: object 0's gain in this block
            b.push(1, 1); // b_default_object_priority
            b.push(2, 2); // render info: as block 0
            b.push(1, 0); // b_additional_table_data_exists
        });
        let element = ObjectAudioMetadataPayload::read(&data)?
            .object_element
            .unwrap();
        let gains: Vec<Vec<i8>> = element
            .object_data
            .iter()
            .map(|o| o.iter().map(|b| b.object_basic_info.object_gain).collect())
            .collect();
        assert_eq!(gains, vec![vec![6, 6], vec![6, 6]]);
        Ok(())
    }

    /// TS 103 420 5.5.1 read literally: (v + 1) << n per extra group, and one group past the
    /// maximum when the stream asks for it.
    #[test]
    fn variable_bits_max_groups() {
        let mut bits = Bits::default();
        bits.push(4, 0xF);
        bits.push(1, 0);
        assert_eq!(
            SliceReader::new(&bits.data)
                .get_variable_bits_max(4, 4)
                .unwrap(),
            0xF
        );

        let mut bits = Bits::default();
        bits.push(4, 1);
        bits.push(1, 1);
        bits.push(4, 2);
        bits.push(1, 0);
        assert_eq!(
            SliceReader::new(&bits.data)
                .get_variable_bits_max(4, 4)
                .unwrap(),
            ((1 + 1) << 4) + 2
        );

        let mut bits = Bits::default();
        for group in 0..5 {
            bits.push(4, 0);
            bits.push(1, (group < 4) as u32);
        }
        let mut reader = SliceReader::new(&bits.data);
        let mut expected = 0;
        for _ in 0..4 {
            expected = (expected << 4) + (1 << 4);
        }
        assert_eq!(reader.get_variable_bits_max(4, 4).unwrap(), expected);
        assert_eq!(reader.position().unwrap(), 25);
    }

    #[test]
    fn trim_lut_values() {
        // Trim index 14 is -15.0 dB (Q12 0x02d8)
        assert_eq!(TRIM_LUT[14], -15.0);
        assert_eq!(TRIM_ATTENUATION_LUT[14], Some(-15.0));
        assert!(TRIM_ATTENUATION_LUT[..4].iter().all(Option::is_none));
        // -1.5 dB step progression from index 7 through 14
        for i in 7..14 {
            assert_eq!(TRIM_LUT[i] - TRIM_LUT[i + 1], 1.5);
        }
    }
}
