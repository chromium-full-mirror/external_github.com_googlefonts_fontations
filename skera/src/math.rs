//! MATH closure and subsetting follow HarfBuzz's hb-ot-math-table.hh.
//! Variants use the cmap closure; per-glyph values use the math closure.
//! Closure and variation collection skip malformed data; serialization fails on it.
//! Child subsetting errors propagate because they also set the serializer error;
//! only unmappable variation devices return an empty result with a null offset.
use crate::{
    layout::variation_index,
    offset::{SerializeSerialize, SerializeSubset},
    serialize::{SerializeErrorFlags, SerializeResultEmpty, Serializer},
    CollectVariationIndices, Plan, Subset, SubsetError, SubsetTable,
};
use read_fonts::{
    collections::IntSet,
    tables::{
        layout::{CoverageTable, DeviceOrVariationIndex},
        math::*,
    },
    types::{GlyphId, Offset16},
    ArrayOfOffsets, FontData, FontRef, MinByteRange, ReadError, TableProvider, TopLevelTable,
};
use write_fonts::FontBuilder;

pub(crate) fn closure(font: &FontRef, glyphs: &mut IntSet<GlyphId>) {
    let Ok(math) = font.math() else { return };
    let Ok(variants) = math.math_variants() else {
        return;
    };
    let mut added = IntSet::empty();
    for (coverage, constructions) in [
        (
            variants.vert_glyph_coverage(),
            variants.vert_glyph_constructions(),
        ),
        (
            variants.horiz_glyph_coverage(),
            variants.horiz_glyph_constructions(),
        ),
    ] {
        let Some(Ok(coverage)) = coverage else {
            continue;
        };
        for (gid, construction) in coverage.iter().zip(constructions.iter()) {
            if !glyphs.contains(gid) {
                continue;
            }
            let Ok(construction) = construction else {
                continue;
            };
            for record in construction.math_glyph_variant_records() {
                added.insert(GlyphId::from(record.variant_glyph()));
            }
            if let Some(Ok(assembly)) = construction.glyph_assembly() {
                for part in assembly.part_records() {
                    added.insert(GlyphId::from(part.glyph_id()));
                }
            }
        }
    }
    // HarfBuzz adds one generation of variants and assembly parts. They are
    // terminal shapes, not additional roots for variant construction.
    glyphs.union(&added);
}

fn read<T>(s: &mut Serializer, result: Result<T, ReadError>) -> Result<T, SerializeErrorFlags> {
    // Unlike HarfBuzz's sanitizer, skera treats malformed required MATH
    // subtable offsets as a failure instead of replacing them with null.
    result.map_err(|_| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR))
}

fn check_len(
    s: &mut Serializer,
    actual: usize,
    expected: usize,
) -> Result<(), SerializeErrorFlags> {
    if actual != expected {
        return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
    }
    Ok(())
}

fn kern_corners<'a>(
    record: &MathKernInfoRecord,
    data: FontData<'a>,
) -> [Option<Result<MathKern<'a>, ReadError>>; 4] {
    [
        record.top_right_math_kern(data),
        record.top_left_math_kern(data),
        record.bottom_right_math_kern(data),
        record.bottom_left_math_kern(data),
    ]
}

fn map_glyph(
    plan: &Plan,
    s: &mut Serializer,
    gid: impl Into<GlyphId>,
) -> Result<u16, SerializeErrorFlags> {
    plan.glyph_map
        .get(&gid.into())
        .and_then(|g| u16::try_from(g.to_u32()).ok())
        .ok_or_else(|| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW))
}

impl<'a> SubsetTable<'a> for MathValueRecord {
    type ArgsForSubset = FontData<'a>;
    type Output = ();

    // MathValueRecord::copy preserves both hint devices and variation indices.
    // Its device offset is relative to the enclosing table, not the record.
    fn subset(
        &self,
        plan: &Plan,
        s: &mut Serializer,
        data: FontData<'a>,
    ) -> Result<(), SerializeErrorFlags> {
        let device = self
            .device(data)
            .transpose()
            .map_err(|_| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR))?;
        let delta = match &device {
            Some(DeviceOrVariationIndex::VariationIndex(index)) => {
                let index = variation_index(index);
                plan.layout_varidx_delta_map
                    .get(&index)
                    .map_or(0, |&(_, delta)| delta)
            }
            _ => 0,
        };
        let value = i32::from(self.value().to_i16())
            .checked_add(delta)
            .and_then(|value| i16::try_from(value).ok())
            .ok_or_else(|| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW))?;
        s.embed(value)?;
        let pos = s.embed(0u16)?;
        if let Some(device) = device {
            Offset16::serialize_subset(&device, s, plan, &plan.layout_varidx_delta_map, pos)
                .is_empty()?;
        }
        Ok(())
    }
}

impl SubsetTable<'_> for MathConstants<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        // Two signed percentages, two minimum heights, 51 MathValueRecords,
        // and the final percentage. Copy the devices with their parent base.
        let start = self.math_leading_byte_range().start;
        let end = self.radical_degree_bottom_raise_percent_byte_range().start;
        s.embed_bytes(&self.min_table_bytes()[..start])?;
        let records: &[MathValueRecord] = read(s, self.offset_data().read_array(start..end))?;
        for record in records {
            record.subset(plan, s, self.offset_data())?;
        }
        s.embed(self.radical_degree_bottom_raise_percent())?;
        Ok(())
    }
}

fn copy_math_record_array(
    coverage: &CoverageTable,
    records: &[MathValueRecord],
    data: FontData,
    plan: &Plan,
    s: &mut Serializer,
) -> Result<(), SerializeErrorFlags> {
    let coverage_pos = s.embed(0u16)?;
    let count_pos = s.embed(0u16)?;
    let mut retained = Vec::new();
    for (gid, record) in coverage.iter().zip(records) {
        if plan.glyphset_mathed.contains(gid) {
            let gid = map_glyph(plan, s, gid)?;
            record.subset(plan, s, data)?;
            retained.push(GlyphId::new(gid as u32));
        }
    }
    s.check_assign::<u16>(
        count_pos,
        retained.len(),
        SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW,
    )?;
    Offset16::serialize_serialize::<CoverageTable>(s, &retained, coverage_pos)
}

impl SubsetTable<'_> for MathItalicsCorrectionInfo<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        let coverage = read(s, self.coverage())?;
        check_len(
            s,
            self.italics_correction().len(),
            self.italics_correction_count() as usize,
        )?;
        copy_math_record_array(
            &coverage,
            self.italics_correction(),
            self.offset_data(),
            plan,
            s,
        )
    }
}

impl SubsetTable<'_> for MathTopAccentAttachment<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        let coverage = read(s, self.top_accent_coverage())?;
        check_len(
            s,
            self.top_accent_attachment().len(),
            self.top_accent_attachment_count() as usize,
        )?;
        copy_math_record_array(
            &coverage,
            self.top_accent_attachment(),
            self.offset_data(),
            plan,
            s,
        )
    }
}

impl SubsetTable<'_> for MathKern<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        let count = self.height_count() as usize;
        check_len(s, self.correction_height().len(), count)?;
        check_len(s, self.kern_values().len(), count + 1)?;
        s.embed(self.height_count())?;
        for record in self.correction_height().iter().chain(self.kern_values()) {
            record.subset(plan, s, self.offset_data())?;
        }
        Ok(())
    }
}

impl SubsetTable<'_> for MathKernInfo<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        let coverage = read(s, self.math_kern_coverage())?;
        check_len(
            s,
            self.math_kern_info_records().len(),
            self.math_kern_count() as usize,
        )?;
        let coverage_pos = s.embed(0u16)?;
        let count_pos = s.embed(0u16)?;
        let mut retained = Vec::new();
        for (gid, record) in coverage.iter().zip(self.math_kern_info_records()) {
            if !plan.glyphset_mathed.contains(gid) {
                continue;
            }
            retained.push(GlyphId::new(map_glyph(plan, s, gid)? as u32));
            for kern in kern_corners(record, self.offset_data()) {
                let pos = s.embed(0u16)?;
                if let Some(kern) = kern {
                    let kern = read(s, kern)?;
                    Offset16::serialize_subset(&kern, s, plan, (), pos)?;
                }
            }
        }
        s.check_assign::<u16>(
            count_pos,
            retained.len(),
            SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW,
        )?;
        Offset16::serialize_serialize::<CoverageTable>(s, &retained, coverage_pos)
    }
}

impl SubsetTable<'_> for MathGlyphInfo<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        let italics_pos = s.embed(0u16)?;
        let accent_pos = s.embed(0u16)?;
        let extended_pos = s.embed(0u16)?;
        let kern_pos = s.embed(0u16)?;
        if let Some(table) = self.math_italics_correction_info() {
            let table = read(s, table)?;
            Offset16::serialize_subset(&table, s, plan, (), italics_pos)?;
        }
        if let Some(table) = self.math_top_accent_attachment() {
            let table = read(s, table)?;
            Offset16::serialize_subset(&table, s, plan, (), accent_pos)?;
        }
        if let Some(coverage) = self.extended_shape_coverage() {
            let coverage = read(s, coverage)?;
            let retained = coverage
                .iter()
                .take(plan.font_num_glyphs)
                .filter(|g| plan.glyphset_mathed.contains(*g))
                .map(|g| map_glyph(plan, s, g).map(|g| GlyphId::new(g as u32)))
                .collect::<Result<Vec<_>, _>>()?;
            if !retained.is_empty() {
                Offset16::serialize_serialize::<CoverageTable>(s, &retained, extended_pos)?;
            }
        }
        if let Some(table) = self.math_kern_info() {
            let table = read(s, table)?;
            Offset16::serialize_subset(&table, s, plan, (), kern_pos)?;
        }
        Ok(())
    }
}

impl SubsetTable<'_> for GlyphAssembly<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        check_len(s, self.part_records().len(), self.part_count() as usize)?;
        self.italics_correction()
            .subset(plan, s, self.offset_data())?;
        s.embed(self.part_count())?;
        for record in self.part_records() {
            let gid = map_glyph(plan, s, record.glyph_id())?;
            s.embed(gid)?;
            s.embed(record.start_connector_length())?;
            s.embed(record.end_connector_length())?;
            s.embed(record.full_advance())?;
            s.embed(record.part_flags())?;
        }
        Ok(())
    }
}

impl SubsetTable<'_> for MathGlyphConstruction<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        check_len(
            s,
            self.math_glyph_variant_records().len(),
            self.variant_count() as usize,
        )?;
        let assembly_pos = s.embed(0u16)?;
        if let Some(assembly) = self.glyph_assembly() {
            let assembly = read(s, assembly)?;
            Offset16::serialize_subset(&assembly, s, plan, (), assembly_pos)?;
        }
        s.embed(self.variant_count())?;
        for record in self.math_glyph_variant_records() {
            let gid = map_glyph(plan, s, record.variant_glyph())?;
            s.embed(gid)?;
            s.embed(record.advance_measurement())?;
        }
        Ok(())
    }
}

fn collect_coverage_and_indices(
    coverage: Option<Result<CoverageTable, ReadError>>,
    count: usize,
    plan: &Plan,
    s: &mut Serializer,
) -> Result<(Vec<GlyphId>, Vec<usize>), SerializeErrorFlags> {
    let mut retained = Vec::new();
    let mut indices = Vec::new();
    if let Some(coverage) = coverage {
        let coverage = read(s, coverage)?;
        for (index, gid) in coverage.iter().take(count).enumerate() {
            if plan.glyphset_cmaped.contains(gid) {
                retained.push(GlyphId::new(map_glyph(plan, s, gid)? as u32));
                indices.push(index);
            }
        }
    }
    Ok((retained, indices))
}

fn serialize_constructions<'a>(
    constructions: ArrayOfOffsets<'a, MathGlyphConstruction<'a>, Offset16>,
    indices: &[usize],
    plan: &Plan,
    s: &mut Serializer,
) -> Result<(), SerializeErrorFlags> {
    for &index in indices {
        let pos = s.embed(0u16)?;
        match constructions.get(index) {
            Err(ReadError::NullOffset) => (),
            table => {
                let table = read(s, table)?;
                Offset16::serialize_subset(&table, s, plan, (), pos)?;
            }
        }
    }
    Ok(())
}

impl SubsetTable<'_> for MathVariants<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<(), SerializeErrorFlags> {
        check_len(
            s,
            self.vert_glyph_construction_offsets().len(),
            self.vert_glyph_count() as usize,
        )?;
        check_len(
            s,
            self.horiz_glyph_construction_offsets().len(),
            self.horiz_glyph_count() as usize,
        )?;
        let (vertical, vert_indices) = collect_coverage_and_indices(
            self.vert_glyph_coverage(),
            self.vert_glyph_count() as usize,
            plan,
            s,
        )?;
        let (horizontal, horiz_indices) = collect_coverage_and_indices(
            self.horiz_glyph_coverage(),
            self.horiz_glyph_count() as usize,
            plan,
            s,
        )?;
        s.embed(self.min_connector_overlap())?;
        let vert_pos = s.embed(0u16)?;
        let horiz_pos = s.embed(0u16)?;
        let vert_count_pos = s.embed(0u16)?;
        let horiz_count_pos = s.embed(0u16)?;
        s.check_assign::<u16>(
            vert_count_pos,
            vertical.len(),
            SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW,
        )?;
        s.check_assign::<u16>(
            horiz_count_pos,
            horizontal.len(),
            SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW,
        )?;
        serialize_constructions(self.vert_glyph_constructions(), &vert_indices, plan, s)?;
        serialize_constructions(self.horiz_glyph_constructions(), &horiz_indices, plan, s)?;
        if !vertical.is_empty() {
            Offset16::serialize_serialize::<CoverageTable>(s, &vertical, vert_pos)?;
        }
        if !horizontal.is_empty() {
            Offset16::serialize_serialize::<CoverageTable>(s, &horizontal, horiz_pos)?;
        }
        Ok(())
    }
}

fn collect_value_indices(
    record: &MathValueRecord,
    data: FontData,
    plan: &Plan,
    indices: &mut IntSet<u32>,
) {
    if let Some(Ok(device)) = record.device(data) {
        device.collect_variation_indices(plan, indices);
    }
}

fn collect_covered_value_indices(
    coverage: Result<CoverageTable, ReadError>,
    records: &[MathValueRecord],
    data: FontData,
    plan: &Plan,
    indices: &mut IntSet<u32>,
) {
    let Ok(coverage) = coverage else { return };
    for (gid, record) in coverage.iter().zip(records) {
        if plan.glyphset_mathed.contains(gid) {
            collect_value_indices(record, data, plan, indices);
        }
    }
}

impl CollectVariationIndices for MathConstants<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        let start = self.math_leading_byte_range().start;
        let end = self.radical_degree_bottom_raise_percent_byte_range().start;
        let Ok(records) = self.offset_data().read_array::<MathValueRecord>(start..end) else {
            return;
        };
        for record in records {
            collect_value_indices(record, self.offset_data(), plan, indices);
        }
    }
}

impl CollectVariationIndices for MathItalicsCorrectionInfo<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        collect_covered_value_indices(
            self.coverage(),
            self.italics_correction(),
            self.offset_data(),
            plan,
            indices,
        );
    }
}

impl CollectVariationIndices for MathTopAccentAttachment<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        collect_covered_value_indices(
            self.top_accent_coverage(),
            self.top_accent_attachment(),
            self.offset_data(),
            plan,
            indices,
        );
    }
}

impl CollectVariationIndices for MathKern<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        for record in self.correction_height().iter().chain(self.kern_values()) {
            collect_value_indices(record, self.offset_data(), plan, indices);
        }
    }
}

impl CollectVariationIndices for MathKernInfo<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        let Ok(coverage) = self.math_kern_coverage() else {
            return;
        };
        for (gid, record) in coverage.iter().zip(self.math_kern_info_records()) {
            if !plan.glyphset_mathed.contains(gid) {
                continue;
            }
            for kern in kern_corners(record, self.offset_data()) {
                if let Some(Ok(kern)) = kern {
                    kern.collect_variation_indices(plan, indices);
                }
            }
        }
    }
}

impl CollectVariationIndices for MathGlyphInfo<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        if let Some(Ok(italics)) = self.math_italics_correction_info() {
            italics.collect_variation_indices(plan, indices);
        }
        if let Some(Ok(accents)) = self.math_top_accent_attachment() {
            accents.collect_variation_indices(plan, indices);
        }
        if let Some(Ok(kern_info)) = self.math_kern_info() {
            kern_info.collect_variation_indices(plan, indices);
        }
    }
}

impl CollectVariationIndices for GlyphAssembly<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        collect_value_indices(self.italics_correction(), self.offset_data(), plan, indices);
    }
}

impl CollectVariationIndices for MathGlyphConstruction<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        if let Some(Ok(assembly)) = self.glyph_assembly() {
            assembly.collect_variation_indices(plan, indices);
        }
    }
}

impl CollectVariationIndices for MathVariants<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        for (coverage, constructions) in [
            (self.vert_glyph_coverage(), self.vert_glyph_constructions()),
            (
                self.horiz_glyph_coverage(),
                self.horiz_glyph_constructions(),
            ),
        ] {
            let Some(Ok(coverage)) = coverage else {
                continue;
            };
            for (gid, construction) in coverage.iter().zip(constructions.iter()) {
                if !plan.glyphset_cmaped.contains(gid) {
                    continue;
                }
                if let Ok(construction) = construction {
                    construction.collect_variation_indices(plan, indices);
                }
            }
        }
    }
}

impl CollectVariationIndices for Math<'_> {
    fn collect_variation_indices(&self, plan: &Plan, indices: &mut IntSet<u32>) {
        if let Ok(constants) = self.math_constants() {
            constants.collect_variation_indices(plan, indices);
        }
        if let Ok(info) = self.math_glyph_info() {
            info.collect_variation_indices(plan, indices);
        }
        if let Ok(variants) = self.math_variants() {
            variants.collect_variation_indices(plan, indices);
        }
    }
}

impl Subset for Math<'_> {
    fn subset(
        &self,
        plan: &Plan,
        _font: &FontRef,
        s: &mut Serializer,
        _builder: &mut FontBuilder,
    ) -> Result<(), SubsetError> {
        let mut subset = || -> Result<(), SerializeErrorFlags> {
            if self.version().major != 1 {
                return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
            }
            s.embed(self.version())?;
            let constants_pos = s.embed(0u16)?;
            let info_pos = s.embed(0u16)?;
            let variants_pos = s.embed(0u16)?;
            if !self.math_constants_offset().is_null() {
                let constants = read(s, self.math_constants())?;
                Offset16::serialize_subset(&constants, s, plan, (), constants_pos)?;
            }
            if !self.math_glyph_info_offset().is_null() {
                let info = read(s, self.math_glyph_info())?;
                Offset16::serialize_subset(&info, s, plan, (), info_pos)?;
            }
            if !self.math_variants_offset().is_null() {
                let variants = read(s, self.math_variants())?;
                Offset16::serialize_subset(&variants, s, plan, (), variants_pos)?;
            }
            Ok(())
        };
        subset().map_err(|_| SubsetError::SubsetTableError(Math::TAG))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use write_fonts::tables::{layout::VariationIndex, math::MathValueRecord as OwnedValue};

    #[test]
    fn value_records_apply_deltas_and_remap_variation_indices() {
        let source = write_fonts::dump_table(&OwnedValue::new(
            10.into(),
            Some(VariationIndex::new(1, 2).into()),
        ))
        .unwrap();
        let source_data = FontData::new(&source);
        let value = source_data.read_array::<MathValueRecord>(0..4).unwrap()[0];
        let mut plan = Plan::default();
        plan.layout_varidx_delta_map.insert(0x10002, (0x30004, 5));
        let mut serializer = Serializer::new(64);
        serializer.start_serialize().unwrap();
        value.subset(&plan, &mut serializer, source_data).unwrap();
        serializer.end_serialize();

        let bytes = serializer.copy_bytes();
        let data = FontData::new(&bytes);
        let value = data.read_array::<MathValueRecord>(0..4).unwrap()[0];
        assert_eq!(value.value().to_i16(), 15);
        let DeviceOrVariationIndex::VariationIndex(index) = value.device(data).unwrap().unwrap()
        else {
            panic!("expected variation index")
        };
        assert_eq!(index.delta_set_outer_index(), 3);
        assert_eq!(index.delta_set_inner_index(), 4);
    }

    #[test]
    fn value_records_drop_unmappable_variation_indices() {
        for (index, mapped, expected_value) in [
            ((1, 2), None, 10),
            ((0xffff, 0xffff), None, 10),
            ((1, 2), Some((0xffffffff, 5)), 15),
        ] {
            let source = write_fonts::dump_table(&OwnedValue::new(
                10.into(),
                Some(VariationIndex::new(index.0, index.1).into()),
            ))
            .unwrap();
            let source_data = FontData::new(&source);
            let value = source_data.read_array::<MathValueRecord>(0..4).unwrap()[0];
            let mut plan = Plan::default();
            if let Some(mapped) = mapped {
                plan.layout_varidx_delta_map.insert(0x10002, mapped);
            }
            let mut serializer = Serializer::new(64);
            serializer.start_serialize().unwrap();
            value.subset(&plan, &mut serializer, source_data).unwrap();
            serializer.end_serialize();

            let bytes = serializer.copy_bytes();
            let data = FontData::new(&bytes);
            let value = data.read_array::<MathValueRecord>(0..4).unwrap()[0];
            assert_eq!(value.value().to_i16(), expected_value);
            assert!(value.device(data).is_none());
        }
    }
}
