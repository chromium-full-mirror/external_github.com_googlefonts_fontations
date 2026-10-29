//! MATH closure and subsetting follow HarfBuzz's hb-ot-math-table.hh.
//! Variants use the cmap closure; per-glyph values use the math closure.
use crate::{
    offset::{SerializeSerialize, SerializeSubset},
    serialize::{SerializeErrorFlags, Serializer},
    CollectVariationIndices, Plan, SubsetError, SubsetTable,
};
use read_fonts::{
    collections::IntSet,
    tables::{layout::CoverageTable, math::*},
    types::{GlyphId, Offset16},
    ArrayOfOffsets, FontData, FontRef, MinByteRange, ReadError, TableProvider, TopLevelTable,
};

type Result<T> = std::result::Result<T, SerializeErrorFlags>;

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

fn read<T>(s: &mut Serializer, result: std::result::Result<T, ReadError>) -> Result<T> {
    result.map_err(|_| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR))
}

fn map_glyph(plan: &Plan, s: &mut Serializer, gid: impl Into<GlyphId>) -> Result<u16> {
    plan.glyph_map
        .get(&gid.into())
        .and_then(|g| u16::try_from(g.to_u32()).ok())
        .ok_or_else(|| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW))
}

// MathValueRecord::copy preserves both hint devices and variation indices.
// Its device offset is relative to the enclosing table, not the record.
fn copy_value(
    record: &MathValueRecord,
    data: FontData,
    plan: &Plan,
    s: &mut Serializer,
) -> Result<()> {
    let device = record
        .device(data)
        .transpose()
        .map_err(|_| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR))?;
    let delta = match &device {
        Some(read_fonts::tables::layout::DeviceOrVariationIndex::VariationIndex(index)) => {
            let index = ((index.delta_set_outer_index() as u32) << 16)
                | index.delta_set_inner_index() as u32;
            plan.layout_varidx_delta_map
                .get(&index)
                .map_or(0, |&(_, delta)| delta)
        }
        _ => 0,
    };
    let value = i32::from(record.value().to_i16())
        .checked_add(delta)
        .and_then(|value| i16::try_from(value).ok())
        .ok_or_else(|| s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_INT_OVERFLOW))?;
    s.embed(value)?;
    let pos = s.embed(0u16)?;
    if let Some(device) = device {
        Offset16::serialize_subset(&device, s, plan, &plan.layout_varidx_delta_map, pos)?;
    }
    Ok(())
}

impl SubsetTable<'_> for MathConstants<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, _plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        // Two signed percentages, two minimum heights, 51 MathValueRecords,
        // and the final percentage. Copy the devices with their parent base.
        s.embed_bytes(&self.min_table_bytes()[..8])?;
        let records: &[MathValueRecord] = read(s, self.offset_data().read_array(8..212))?;
        for record in records {
            copy_value(record, self.offset_data(), _plan, s)?;
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
) -> Result<()> {
    let coverage_pos = s.embed(0u16)?;
    let count_pos = s.embed(0u16)?;
    let mut retained = Vec::new();
    for (gid, record) in coverage.iter().zip(records) {
        if plan.glyphset_mathed.contains(gid) {
            let gid = map_glyph(plan, s, gid)?;
            copy_value(record, data, plan, s)?;
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
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        let coverage = read(s, self.coverage())?;
        if self.italics_correction().len() != self.italics_correction_count() as usize {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
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
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        let coverage = read(s, self.top_accent_coverage())?;
        if self.top_accent_attachment().len() != self.top_accent_attachment_count() as usize {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
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
    fn subset(&self, _plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        let count = self.height_count() as usize;
        if self.correction_height().len() != count || self.kern_values().len() != count + 1 {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
        s.embed(self.height_count())?;
        for record in self.correction_height().iter().chain(self.kern_values()) {
            copy_value(record, self.offset_data(), _plan, s)?;
        }
        Ok(())
    }
}

impl SubsetTable<'_> for MathKernInfo<'_> {
    type ArgsForSubset = ();
    type Output = ();
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        let coverage = read(s, self.math_kern_coverage())?;
        if self.math_kern_info_records().len() != self.math_kern_count() as usize {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
        let coverage_pos = s.embed(0u16)?;
        let count_pos = s.embed(0u16)?;
        let mut retained = Vec::new();
        for (gid, record) in coverage.iter().zip(self.math_kern_info_records()) {
            if !plan.glyphset_mathed.contains(gid) {
                continue;
            }
            retained.push(GlyphId::new(map_glyph(plan, s, gid)? as u32));
            for kern in [
                record.top_right_math_kern(self.offset_data()),
                record.top_left_math_kern(self.offset_data()),
                record.bottom_right_math_kern(self.offset_data()),
                record.bottom_left_math_kern(self.offset_data()),
            ] {
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
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
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
                .collect::<Result<Vec<_>>>()?;
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
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        if self.part_records().len() != self.part_count() as usize {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
        copy_value(self.italics_correction(), self.offset_data(), plan, s)?;
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
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
        if self.math_glyph_variant_records().len() != self.variant_count() as usize {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
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
    coverage: Option<std::result::Result<CoverageTable, ReadError>>,
    count: usize,
    plan: &Plan,
    s: &mut Serializer,
) -> Result<(Vec<GlyphId>, Vec<usize>)> {
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
) -> Result<()> {
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
    fn subset(&self, plan: &Plan, s: &mut Serializer, _: ()) -> Result<()> {
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
        if self.vert_glyph_construction_offsets().len() != self.vert_glyph_count() as usize
            || self.horiz_glyph_construction_offsets().len() != self.horiz_glyph_count() as usize
        {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
        s.embed(self.min_connector_overlap())?;
        let vert_pos = s.embed(0u16)?;
        let horiz_pos = s.embed(0u16)?;
        s.embed(vertical.len() as u16)?;
        s.embed(horizontal.len() as u16)?;
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

fn collect_covered_values(
    coverage: std::result::Result<CoverageTable, ReadError>,
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

fn collect_kern_indices(kern: &MathKern, plan: &Plan, indices: &mut IntSet<u32>) {
    for record in kern.correction_height().iter().chain(kern.kern_values()) {
        collect_value_indices(record, kern.offset_data(), plan, indices);
    }
}

pub(crate) fn collect_variation_indices(math: &Math, plan: &Plan, indices: &mut IntSet<u32>) {
    if let Ok(constants) = math.math_constants() {
        if let Ok(records) = constants
            .offset_data()
            .read_array::<MathValueRecord>(8..212)
        {
            for record in records {
                collect_value_indices(record, constants.offset_data(), plan, indices);
            }
        }
    }

    if let Ok(info) = math.math_glyph_info() {
        if let Some(Ok(italics)) = info.math_italics_correction_info() {
            collect_covered_values(
                italics.coverage(),
                italics.italics_correction(),
                italics.offset_data(),
                plan,
                indices,
            );
        }
        if let Some(Ok(accents)) = info.math_top_accent_attachment() {
            collect_covered_values(
                accents.top_accent_coverage(),
                accents.top_accent_attachment(),
                accents.offset_data(),
                plan,
                indices,
            );
        }
        if let Some(Ok(kern_info)) = info.math_kern_info() {
            if let Ok(coverage) = kern_info.math_kern_coverage() {
                for (gid, record) in coverage.iter().zip(kern_info.math_kern_info_records()) {
                    if !plan.glyphset_mathed.contains(gid) {
                        continue;
                    }
                    for kern in [
                        record.top_right_math_kern(kern_info.offset_data()),
                        record.top_left_math_kern(kern_info.offset_data()),
                        record.bottom_right_math_kern(kern_info.offset_data()),
                        record.bottom_left_math_kern(kern_info.offset_data()),
                    ] {
                        if let Some(Ok(kern)) = kern {
                            collect_kern_indices(&kern, plan, indices);
                        }
                    }
                }
            }
        }
    }

    let Ok(variants) = math.math_variants() else {
        return;
    };
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
            if !plan.glyphset_cmaped.contains(gid) {
                continue;
            }
            let Ok(construction) = construction else {
                continue;
            };
            if let Some(Ok(assembly)) = construction.glyph_assembly() {
                collect_value_indices(
                    assembly.italics_correction(),
                    assembly.offset_data(),
                    plan,
                    indices,
                );
            }
        }
    }
}

pub(crate) fn subset(
    font: &FontRef,
    plan: &Plan,
    s: &mut Serializer,
) -> std::result::Result<(), SubsetError> {
    let result = (|| {
        let table = read(s, font.math())?;
        if table.version().major != 1 {
            return Err(s.set_err(SerializeErrorFlags::SERIALIZE_ERROR_READ_ERROR));
        }
        s.embed(table.version())?;
        let constants_pos = s.embed(0u16)?;
        let info_pos = s.embed(0u16)?;
        let variants_pos = s.embed(0u16)?;
        if !table.math_constants_offset().is_null() {
            let constants = read(s, table.math_constants())?;
            Offset16::serialize_subset(&constants, s, plan, (), constants_pos)?;
        }
        if !table.math_glyph_info_offset().is_null() {
            let info = read(s, table.math_glyph_info())?;
            Offset16::serialize_subset(&info, s, plan, (), info_pos)?;
        }
        if !table.math_variants_offset().is_null() {
            let variants = read(s, table.math_variants())?;
            Offset16::serialize_subset(&variants, s, plan, (), variants_pos)?;
        }
        Ok(())
    })();
    result.map_err(|_| SubsetError::SubsetTableError(Math::TAG))
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
        copy_value(&value, FontData::new(&source), &plan, &mut serializer).unwrap();
        serializer.end_serialize();

        let bytes = serializer.copy_bytes();
        let data = FontData::new(&bytes);
        let value = data.read_array::<MathValueRecord>(0..4).unwrap()[0];
        assert_eq!(value.value().to_i16(), 15);
        let read_fonts::tables::layout::DeviceOrVariationIndex::VariationIndex(index) =
            value.device(data).unwrap().unwrap()
        else {
            panic!("expected variation index")
        };
        assert_eq!(index.delta_set_outer_index(), 3);
        assert_eq!(index.delta_set_inner_index(), 4);
    }
}
