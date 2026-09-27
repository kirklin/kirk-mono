use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use write_fonts::FontBuilder;
use write_fonts::from_obj::ToOwnedTable;
use write_fonts::read::tables::layout::{FeatureList, FeatureParams};
use write_fonts::read::{FontRef, TableProvider};
use write_fonts::tables::fvar::Fvar;
use write_fonts::tables::gasp::{Gasp, GaspRange, GaspRangeBehavior};
use write_fonts::tables::head::{Head, MacStyle};
use write_fonts::tables::name::{Name, NameRecord};
use write_fonts::tables::os2::{Os2, SelectionFlags};
use write_fonts::tables::stat::{AxisRecord, AxisValue, AxisValueTableFlags, Stat};
use write_fonts::types::{Fixed, NameId, Tag};

/// Named instances in axis order, with the OS/2 weight class of each.
const WEIGHTS: [(&str, u16); 8] = [
    ("Thin", 100),
    ("ExtraLight", 200),
    ("Light", 300),
    ("Regular", 400),
    ("Medium", 500),
    ("SemiBold", 600),
    ("Bold", 700),
    ("ExtraBold", 800),
];

/// Copyright, trademark, manufacturer, designer, description, URL and license strings.
const FORBIDDEN_NAME_IDS: [u16; 9] = [0, 7, 8, 9, 10, 11, 12, 13, 14];

/// Dropout control for outlines without hinting instructions: SCANCTRL 0x01FF, SCANTYPE 4.
const UNHINTED_PREP: [u8; 7] = [0xb8, 0x01, 0xff, 0x85, 0xb0, 0x04, 0x8d];

pub struct Instance {
    pub weight: &'static str,
    pub class: u16,
    /// Where the source places this instance on the variable font's weight axis.
    pub location: f64,
}

pub fn style_name(weight: &str, italic: bool) -> String {
    match (weight, italic) {
        (weight, false) => weight.to_string(),
        ("Regular", true) => "Italic".to_string(),
        (weight, true) => format!("{weight} Italic"),
    }
}

pub fn postscript_family(family: &str) -> String {
    family.replace(' ', "")
}

/// Names the compiled variable font, moves its named instances to their weight classes and
/// writes the STAT table. Returns the font together with the source instance locations.
pub fn finalize_variable(data: &[u8], family: &str, italic: bool) -> Result<(Vec<u8>, Vec<Instance>)> {
    let font = FontRef::new(data)?;
    let mut name: Name = font.name()?.to_owned_table();
    check_forbidden_names(&name)?;

    let mut fvar: Fvar = font.fvar()?.to_owned_table();
    let mut instances = Vec::new();
    for record in &mut fvar.axis_instance_arrays.instances {
        let subfamily = name_string(&name, record.subfamily_name_id)?;
        let weight_name = match subfamily.as_str() {
            "Italic" => "Regular",
            other => other.strip_suffix(" Italic").unwrap_or(other),
        };
        let &(weight, class) = WEIGHTS
            .iter()
            .find(|(weight, _)| *weight == weight_name)
            .with_context(|| format!("unexpected named instance '{subfamily}'"))?;
        instances.push(Instance { weight, class, location: record.coordinates[0].to_f64() });
        record.coordinates[0] = Fixed::from_f64(class.into());
    }
    ensure!(
        instances.iter().map(|instance| instance.weight).eq(WEIGHTS.iter().map(|(weight, _)| *weight)),
        "named instances must be {:?} in axis order",
        WEIGHTS.map(|(weight, _)| weight)
    );

    let (version, vendor) = version_and_vendor(&font)?;
    let style = if italic { "Italic" } else { "Regular" };
    let postscript = format!("{}-{style}", postscript_family(family));
    set_name(&mut name, 1, family);
    set_name(&mut name, 2, style);
    set_name(&mut name, 3, &format!("{version};{vendor};{postscript}"));
    set_name(&mut name, 4, &format!("{family} {style}"));
    set_name(&mut name, 5, &format!("Version {version}"));
    set_name(&mut name, 6, &postscript);
    set_name(&mut name, 25, &format!("{}{}", postscript_family(family), if italic { "Italic" } else { "" }));

    let weight_axis = fvar
        .axis_instance_arrays
        .axes
        .iter()
        .find(|axis| axis.axis_tag == Tag::new(b"wght"))
        .context("font has no wght axis")?
        .axis_name_id;
    let italic_axis = name_id_for(&mut name, "Italic");
    let bold_class = WEIGHTS.iter().find(|(weight, _)| *weight == "Bold").map(|(_, class)| *class).unwrap();
    let mut values = Vec::new();
    for (weight, class) in WEIGHTS {
        let value_name = name_id_for(&mut name, weight);
        values.push(if weight == "Regular" {
            AxisValue::format_3(
                0,
                AxisValueTableFlags::ELIDABLE_AXIS_VALUE_NAME,
                value_name,
                Fixed::from_f64(class.into()),
                Fixed::from_f64(bold_class.into()),
            )
        } else {
            AxisValue::format_1(0, AxisValueTableFlags::empty(), value_name, Fixed::from_f64(class.into()))
        });
    }
    values.push(if italic {
        AxisValue::format_1(1, AxisValueTableFlags::empty(), italic_axis, Fixed::from_f64(1.0))
    } else {
        AxisValue::format_3(
            1,
            AxisValueTableFlags::ELIDABLE_AXIS_VALUE_NAME,
            name_id_for(&mut name, "Roman"),
            Fixed::from_f64(0.0),
            Fixed::from_f64(1.0),
        )
    });
    let stat = Stat::new(
        vec![AxisRecord::new(Tag::new(b"wght"), weight_axis, 0), AxisRecord::new(Tag::new(b"ital"), italic_axis, 1)],
        values,
        NameId::new(2),
    );

    let mut head: Head = font.head()?.to_owned_table();
    head.modified = head.created;
    name.name_record.sort();

    let mut builder = FontBuilder::new();
    builder.add_table(&name)?.add_table(&fvar)?.add_table(&stat)?.add_table(&head)?;
    Ok((rebuild(&font, &mut builder, &[]), instances))
}

/// Adds the grid-fitting and dropout-control settings for fonts that carry no hinting.
pub fn add_unhinted_rendering_tables(data: &[u8]) -> Result<Vec<u8>> {
    let font = FontRef::new(data)?;
    for tag in [b"gasp", b"prep", b"fpgm", b"cvt "] {
        ensure!(font.data_for_tag(Tag::new(tag)).is_none(), "font already has a '{}' table", Tag::new(tag));
    }
    let behavior = GaspRangeBehavior::GASP_GRIDFIT
        | GaspRangeBehavior::GASP_DOGRAY
        | GaspRangeBehavior::GASP_SYMMETRIC_GRIDFIT
        | GaspRangeBehavior::GASP_SYMMETRIC_SMOOTHING;
    let gasp = Gasp::new(1, 1, vec![GaspRange::new(0xFFFF, behavior)]);

    let mut builder = FontBuilder::new();
    builder.add_table(&gasp)?.add_raw(Tag::new(b"prep"), UNHINTED_PREP.as_slice());
    Ok(rebuild(&font, &mut builder, &[]))
}

/// Names a static instance cut from the variable font and sets its style-linking flags.
pub fn finalize_static(data: &[u8], family: &str, instance: &Instance, italic: bool) -> Result<Vec<u8>> {
    let font = FontRef::new(data)?;
    let name = static_names(&font, family, instance.weight, italic)?;

    let bold = instance.weight == "Bold";
    let mut os2: Os2 = font.os2()?.to_owned_table();
    os2.us_weight_class = instance.class;
    os2.fs_selection.remove(SelectionFlags::ITALIC | SelectionFlags::BOLD | SelectionFlags::REGULAR);
    if italic {
        os2.fs_selection.insert(SelectionFlags::ITALIC);
    }
    if bold {
        os2.fs_selection.insert(SelectionFlags::BOLD);
    }
    if !italic && !bold {
        os2.fs_selection.insert(SelectionFlags::REGULAR);
    }

    let mut head: Head = font.head()?.to_owned_table();
    head.mac_style.remove(MacStyle::BOLD | MacStyle::ITALIC);
    if bold {
        head.mac_style.insert(MacStyle::BOLD);
    }
    if italic {
        head.mac_style.insert(MacStyle::ITALIC);
    }
    head.modified = head.created;

    let mut builder = FontBuilder::new();
    builder.add_table(&name)?.add_table(&os2)?.add_table(&head)?;
    Ok(rebuild(&font, &mut builder, &[Tag::new(b"STAT")]))
}

/// Renames a finished static font into another family, keeping every other table.
pub fn rename_static(data: &[u8], family: &str, instance: &Instance, italic: bool) -> Result<Vec<u8>> {
    let font = FontRef::new(data)?;
    let name = static_names(&font, family, instance.weight, italic)?;

    let mut builder = FontBuilder::new();
    builder.add_table(&name)?;
    Ok(rebuild(&font, &mut builder, &[]))
}

/// Resets the modification date that ttfautohint sets to the current time.
pub fn stamp(data: &[u8]) -> Result<Vec<u8>> {
    let font = FontRef::new(data)?;
    let mut head: Head = font.head()?.to_owned_table();
    head.modified = head.created;

    let mut builder = FontBuilder::new();
    builder.add_table(&head)?;
    Ok(rebuild(&font, &mut builder, &[]))
}

fn static_names(font: &FontRef, family: &str, weight: &str, italic: bool) -> Result<Name> {
    let source: Name = font.name()?.to_owned_table();
    check_forbidden_names(&source)?;
    let referenced = referenced_name_ids(font)?;
    let mut name = Name::new(
        source.name_record.into_iter().filter(|record| referenced.contains(&record.name_id.to_u16())).collect(),
    );

    let (version, vendor) = version_and_vendor(font)?;
    let style = style_name(weight, italic);
    let postscript = format!("{}-{}", postscript_family(family), style.replace(' ', ""));
    if matches!(weight, "Regular" | "Bold") {
        set_name(&mut name, 1, family);
        set_name(&mut name, 2, &style);
    } else {
        set_name(&mut name, 1, &format!("{family} {weight}"));
        set_name(&mut name, 2, if italic { "Italic" } else { "Regular" });
        set_name(&mut name, 16, family);
        set_name(&mut name, 17, &style);
    }
    set_name(&mut name, 3, &format!("{version};{vendor};{postscript}"));
    set_name(&mut name, 4, &format!("{family} {style}"));
    set_name(&mut name, 5, &format!("Version {version}"));
    set_name(&mut name, 6, &postscript);
    name.name_record.sort();
    Ok(name)
}

/// Name IDs that layout feature parameters point at, such as stylistic set labels.
fn referenced_name_ids(font: &FontRef) -> Result<BTreeSet<u16>> {
    let mut ids = BTreeSet::new();
    if font.data_for_tag(Tag::new(b"GSUB")).is_some() {
        collect_feature_name_ids(&font.gsub()?.feature_list()?, &mut ids)?;
    }
    if font.data_for_tag(Tag::new(b"GPOS")).is_some() {
        collect_feature_name_ids(&font.gpos()?.feature_list()?, &mut ids)?;
    }
    ids.remove(&0);
    Ok(ids)
}

fn collect_feature_name_ids(list: &FeatureList, ids: &mut BTreeSet<u16>) -> Result<()> {
    for record in list.feature_records() {
        match record.feature(list.offset_data())?.feature_params().transpose()? {
            Some(FeatureParams::StylisticSet(params)) => {
                ids.insert(params.ui_name_id().to_u16());
            }
            Some(FeatureParams::CharacterVariant(params)) => {
                ids.extend(
                    [params.feat_ui_label_name_id(), params.feat_ui_tooltip_text_name_id(), params.sample_text_name_id()]
                        .map(NameId::to_u16),
                );
                let first = params.first_param_ui_label_name_id().to_u16();
                ids.extend((0..params.num_named_parameters()).map(|offset| first + offset));
            }
            Some(FeatureParams::Size(_)) | None => {}
        }
    }
    Ok(())
}

fn version_and_vendor(font: &FontRef) -> Result<(String, String)> {
    let version = format!("{:.3}", font.head()?.font_revision().to_f64());
    let vendor = font.os2()?.ach_vend_id().to_string().trim().to_string();
    Ok((version, vendor))
}

fn check_forbidden_names(name: &Name) -> Result<()> {
    if let Some(record) = name.name_record.iter().find(|record| FORBIDDEN_NAME_IDS.contains(&record.name_id.to_u16())) {
        bail!("name ID {} ('{}') must not come from the sources", record.name_id, record.string.as_str());
    }
    Ok(())
}

fn name_string(name: &Name, id: NameId) -> Result<String> {
    name.name_record
        .iter()
        .find(|record| record.platform_id == 3 && record.name_id == id)
        .map(|record| record.string.as_str().to_string())
        .with_context(|| format!("name ID {id} is missing"))
}

fn set_name(name: &mut Name, id: u16, text: &str) {
    name.name_record.retain(|record| record.name_id.to_u16() != id);
    name.name_record.push(NameRecord::new(3, 1, 0x409, NameId::new(id), text.to_string().into()));
}

/// Reuses a font-specific name record with this text, or adds one.
fn name_id_for(name: &mut Name, text: &str) -> NameId {
    if let Some(record) = name.name_record.iter().find(|record| record.name_id.to_u16() >= 256 && record.string.as_str() == text) {
        return record.name_id;
    }
    let next = name.name_record.iter().map(|record| record.name_id.to_u16()).max().unwrap_or(0).max(255) + 1;
    set_name(name, next, text);
    NameId::new(next)
}

/// Assembles the builder's tables plus every other table of `font` except `drop`.
fn rebuild<'a>(font: &FontRef<'a>, builder: &mut FontBuilder<'a>, drop: &[Tag]) -> Vec<u8> {
    for record in font.table_directory().table_records() {
        let tag = record.tag();
        if !builder.contains(tag) && !drop.contains(&tag) {
            builder.add_raw(tag, font.data_for_tag(tag).expect("table listed in the directory").as_bytes());
        }
    }
    builder.build()
}
