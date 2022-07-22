use crate::coords;
use crate::geodata::find_polygons::{find_polygons_in_multipolygon, NodeDesc, NodeDescPair};
use crate::geodata::saver::save_to_internal_format;
use anyhow::{anyhow, bail, Context, Result};
#[cfg(feature = "pbf")]
use osmpbf::{Element, ElementReader, RelMemberType};
use std::collections::HashSet;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::File;
use std::io::prelude::*;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use xml::attribute::OwnedAttribute;
use xml::reader::{EventReader, XmlEvent};

pub fn import(input: &str, output: &str) -> Result<()> {
    let output_file = File::create(output).context(format!("Failed to open {} for writing", output))?;

    let mut writer = BufWriter::new(output_file);
    let path = Path::new(input);

    let parsed = match path.extension().and_then(OsStr::to_str) {
        Some("osm") | Some("xml") => {
            let input_file = File::open(input).context(format!("Failed to open {} for reading", input))?;
            let parser = EventReader::new(BufReader::new(input_file));
            parse_osm_xml(parser)?
        }
        #[cfg(feature = "pbf")]
        Some("pbf") => parse_pbf(input)?,
        _ => bail!("Extension not supported"),
    };

    println!("Converting geodata to internal format");
    save_to_internal_format(&mut writer, &parsed).context("Failed to write the imported data to the output file")?;
    Ok(())
}

pub(super) struct OsmEntityStorage<E: Default> {
    global_id_to_local_id: HashMap<u64, usize>,
    entities: Vec<E>,
}

impl<E: Default> OsmEntityStorage<E> {
    fn new() -> OsmEntityStorage<E> {
        OsmEntityStorage {
            global_id_to_local_id: HashMap::new(),
            entities: Vec::new(),
        }
    }

    fn add(&mut self, global_id: u64, entity: E) {
        let old_size = self.entities.len();
        self.global_id_to_local_id.insert(global_id, old_size);
        self.entities.push(entity);
    }

    fn translate_id(&self, global_id: u64) -> Option<usize> {
        self.global_id_to_local_id.get(&global_id).cloned()
    }

    pub(super) fn get_entities(&self) -> &Vec<E> {
        &self.entities
    }
}

pub(super) struct EntityStorages {
    pub(super) node_storage: OsmEntityStorage<RawNode>,
    pub(super) way_storage: OsmEntityStorage<RawWay>,
    pub(super) polygon_storage: Vec<Polygon>,
    pub(super) multipolygon_storage: OsmEntityStorage<Multipolygon>,
}

fn print_storage_stats(entity_storages: &EntityStorages) {
    println!(
        "Got {} nodes, {} ways and {} multipolygon relations so far",
        entity_storages.node_storage.entities.len(),
        entity_storages.way_storage.entities.len(),
        entity_storages.multipolygon_storage.entities.len()
    );
}

#[cfg(feature = "pbf")]
fn parse_pbf(input: &str) -> Result<EntityStorages> {
    let mut entity_storages = EntityStorages {
        node_storage: OsmEntityStorage::new(),
        way_storage: OsmEntityStorage::new(),
        polygon_storage: Vec::new(),
        multipolygon_storage: OsmEntityStorage::new(),
    };

    let mut elem_count = 0;
    println!("Parsing PBF");

    let reader = ElementReader::from_path(input)?;
    reader.for_each(|element| {
        match element {
            Element::DenseNode(el_node) => {
                let mut node = RawNode {
                    global_id: el_node.id() as u64,
                    lat: el_node.lat(),
                    lon: el_node.lon(),
                    tags: RawTags::default(),
                };
                for (key, value) in el_node.tags() {
                    node.tags.insert(key.to_string(), value.to_string());
                }
                elem_count += 1;
                entity_storages.node_storage.add(node.global_id, node);
            }
            Element::Way(el_way) => {
                let mut way = RawWay {
                    global_id: el_way.id() as u64,
                    node_ids: RawRefs::default(),
                    tags: RawTags::default(),
                };
                for (key, value) in el_way.tags() {
                    way.tags.insert(key.to_string(), value.to_string());
                }
                for r in el_way.refs() {
                    if let Some(local_id) = entity_storages.node_storage.translate_id(r as u64) {
                        way.node_ids.push(local_id);
                    }
                }
                postprocess_node_refs(&mut way.node_ids);
                elem_count += 1;
                entity_storages.way_storage.add(way.global_id, way);
            }
            Element::Relation(el_rel) => {
                let mut relation = RawRelation {
                    global_id: el_rel.id() as u64,
                    way_refs: Vec::<RelationWayRef>::default(),
                    tags: RawTags::default(),
                };
                for (key, value) in el_rel.tags() {
                    relation.tags.insert(key.to_string(), value.to_string());
                }
                for way in el_rel.members() {
                    if way.member_type == RelMemberType::Way {
                        if let Some(local_id) = entity_storages.way_storage.translate_id(way.member_id as u64) {
                            let is_inner = way.role().unwrap() == "inner";
                            relation.way_refs.push(RelationWayRef {
                                way_id: local_id,
                                is_inner,
                            });
                        }
                    }
                }
                if relation.tags.iter().any(|(k, v)| k == "type" && v == "multipolygon") {
                    let segments = relation.to_segments(&entity_storages);
                    if let Some(polygons) = find_polygons_in_multipolygon(relation.global_id, &segments) {
                        let mut multipolygon = Multipolygon {
                            global_id: relation.global_id,
                            polygon_ids: Vec::new(),
                            tags: relation.tags,
                        };
                        for poly in polygons {
                            multipolygon.polygon_ids.push(entity_storages.polygon_storage.len());
                            entity_storages.polygon_storage.push(poly);
                        }
                        elem_count += 1;
                        entity_storages
                            .multipolygon_storage
                            .add(relation.global_id, multipolygon);
                    }
                }
            }
            Element::Node(_) => panic!(),
        }
        if elem_count % 100_000 == 0 {
            print_storage_stats(&entity_storages);
        }
    })?;

    print_storage_stats(&entity_storages);

    Ok(entity_storages)
}

fn parse_osm_xml<R: Read>(mut parser: EventReader<R>) -> Result<EntityStorages> {
    let mut entity_storages = EntityStorages {
        node_storage: OsmEntityStorage::new(),
        way_storage: OsmEntityStorage::new(),
        polygon_storage: Vec::new(),
        multipolygon_storage: OsmEntityStorage::new(),
    };

    let mut elem_count = 0;

    println!("Parsing XML");
    loop {
        let e = parser.next().context("Failed to parse the input file")?;
        match e {
            XmlEvent::EndDocument => break,
            XmlEvent::StartElement { name, attributes, .. } => {
                process_element(&name.local_name, &attributes, &mut entity_storages, &mut parser)?;
                elem_count += 1;
                if elem_count % 100_000 == 0 {
                    print_storage_stats(&entity_storages);
                }
            }
            _ => {}
        }
    }

    print_storage_stats(&entity_storages);

    Ok(entity_storages)
}

fn process_element<R: Read>(
    name: &str,
    attrs: &[OwnedAttribute],
    entity_storages: &mut EntityStorages,
    parser: &mut EventReader<R>,
) -> Result<()> {
    match name {
        "node" => {
            let mut node = RawNode {
                global_id: get_id(name, attrs)?,
                lat: parse_required_attr(name, attrs, "lat")?,
                lon: parse_required_attr(name, attrs, "lon")?,
                tags: RawTags::default(),
            };
            process_subelements(name, &mut node, entity_storages, process_node_subelement, parser)?;
            entity_storages.node_storage.add(node.global_id, node);
        }
        "way" => {
            let mut way = RawWay {
                global_id: get_id(name, attrs)?,
                node_ids: RawRefs::default(),
                tags: RawTags::default(),
            };
            process_subelements(name, &mut way, entity_storages, process_way_subelement, parser)?;
            postprocess_node_refs(&mut way.node_ids);
            entity_storages.way_storage.add(way.global_id, way);
        }
        "relation" => {
            let mut relation = RawRelation {
                global_id: get_id(name, attrs)?,
                way_refs: Vec::<RelationWayRef>::default(),
                tags: RawTags::default(),
            };
            process_subelements(
                name,
                &mut relation,
                entity_storages,
                process_relation_subelement,
                parser,
            )?;
            if relation.tags.iter().any(|(k, v)| k == "type" && v == "multipolygon") {
                let segments = relation.to_segments(entity_storages);
                if let Some(polygons) = find_polygons_in_multipolygon(relation.global_id, &segments) {
                    let mut multipolygon = Multipolygon {
                        global_id: relation.global_id,
                        polygon_ids: Vec::new(),
                        tags: relation.tags,
                    };
                    for poly in polygons {
                        multipolygon.polygon_ids.push(entity_storages.polygon_storage.len());
                        entity_storages.polygon_storage.push(poly);
                    }
                    entity_storages
                        .multipolygon_storage
                        .add(relation.global_id, multipolygon);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn process_subelements<E: Default, R: Read, F>(
    entity_name: &str,
    entity: &mut E,
    entity_storages: &EntityStorages,
    subelement_processor: F,
    parser: &mut EventReader<R>,
) -> Result<()>
where
    F: Fn(&mut E, &EntityStorages, &str, &[OwnedAttribute]) -> Result<()>,
{
    loop {
        let e = parser.next().context(format!(
            "Failed to parse the input file when processing {}",
            entity_name
        ))?;
        match e {
            XmlEvent::EndDocument => break,
            XmlEvent::EndElement { ref name } if name.local_name == *entity_name => break,
            XmlEvent::StartElement { name, attributes, .. } => {
                subelement_processor(entity, entity_storages, &name.local_name, &attributes)?
            }
            _ => {}
        }
    }
    Ok(())
}

fn postprocess_node_refs(refs: &mut RawRefs) {
    if refs.is_empty() {
        return;
    }

    let mut seen_node_pairs = HashSet::<(usize, usize)>::default();
    let mut refs_without_duplicates = vec![refs[0]];

    for idx in 1..refs.len() {
        let cur = refs[idx];
        let prev = refs[idx - 1];
        let node_pair = (cur, prev);
        if !seen_node_pairs.contains(&node_pair) && !seen_node_pairs.contains(&(prev, cur)) {
            seen_node_pairs.insert(node_pair);
            refs_without_duplicates.push(cur);
        }
    }

    *refs = refs_without_duplicates;
}

fn process_node_subelement(
    node: &mut RawNode,
    _: &EntityStorages,
    sub_name: &str,
    sub_attrs: &[OwnedAttribute],
) -> Result<()> {
    try_add_tag(sub_name, sub_attrs, &mut node.tags).map(|_| ())
}

fn process_way_subelement(
    way: &mut RawWay,
    entity_storages: &EntityStorages,
    sub_name: &str,
    sub_attrs: &[OwnedAttribute],
) -> Result<()> {
    if try_add_tag(sub_name, sub_attrs, &mut way.tags)? {
        return Ok(());
    }
    if sub_name == "nd" {
        if let Some(r) = get_ref(sub_name, sub_attrs, &entity_storages.node_storage)? {
            way.node_ids.push(r);
        }
    }
    Ok(())
}

fn process_relation_subelement(
    relation: &mut RawRelation,
    entity_storages: &EntityStorages,
    sub_name: &str,
    sub_attrs: &[OwnedAttribute],
) -> Result<()> {
    if try_add_tag(sub_name, sub_attrs, &mut relation.tags)? {
        return Ok(());
    }
    if sub_name == "member" && get_required_attr(sub_name, sub_attrs, "type")? == "way" {
        if let Some(r) = get_ref(sub_name, sub_attrs, &entity_storages.way_storage)? {
            let is_inner = get_required_attr(sub_name, sub_attrs, "role")? == "inner";
            relation.way_refs.push(RelationWayRef { way_id: r, is_inner });
        }
    }
    Ok(())
}

fn get_required_attr<'a>(elem_name: &str, attrs: &'a [OwnedAttribute], attr_name: &str) -> Result<&'a String> {
    attrs
        .iter()
        .filter(|x| x.name.local_name == attr_name)
        .map(|x| &x.value)
        .next()
        .ok_or_else(|| anyhow!("Element {} doesn't have required attribute: {}", elem_name, attr_name))
}

fn parse_required_attr<T>(elem_name: &str, attrs: &[OwnedAttribute], attr_name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let value = get_required_attr(elem_name, attrs, attr_name)?;

    let parsed_value = value.parse::<T>().context(format!(
        "Failed to parse the value of attribute {} ({}) for element {}",
        attr_name, value, elem_name
    ))?;

    Ok(parsed_value)
}

fn get_ref<E: Default>(
    elem_name: &str,
    attrs: &[OwnedAttribute],
    storage: &OsmEntityStorage<E>,
) -> Result<Option<usize>> {
    let reference = parse_required_attr(elem_name, attrs, "ref")?;
    Ok(storage.translate_id(reference))
}

fn try_add_tag<'a>(elem_name: &str, attrs: &'a [OwnedAttribute], tags: &mut RawTags) -> Result<bool> {
    if elem_name != "tag" {
        return Ok(false);
    }
    let key = get_required_attr(elem_name, attrs, "k")?;
    let value = get_required_attr(elem_name, attrs, "v")?;
    tags.insert(key.clone(), value.clone());
    Ok(true)
}

fn get_id(elem_name: &str, attrs: &[OwnedAttribute]) -> Result<u64> {
    parse_required_attr(elem_name, attrs, "id")
}

pub(super) type RawRefs = Vec<usize>;
pub(super) type RawTags = BTreeMap<String, String>;

#[derive(Default)]
pub(super) struct RawNode {
    pub(super) global_id: u64,
    pub(super) lat: f64,
    pub(super) lon: f64,
    pub(super) tags: RawTags,
}

impl coords::Coords for RawNode {
    fn lat(&self) -> f64 {
        self.lat
    }

    fn lon(&self) -> f64 {
        self.lon
    }
}

#[derive(Default)]
pub(super) struct RawWay {
    pub(super) global_id: u64,
    pub(super) node_ids: RawRefs,
    pub(super) tags: RawTags,
}

pub struct RelationWayRef {
    way_id: usize,
    is_inner: bool,
}

#[derive(Default)]
struct RawRelation {
    global_id: u64,
    way_refs: Vec<RelationWayRef>,
    tags: RawTags,
}

impl RawRelation {
    fn to_segments(&self, entity_storages: &EntityStorages) -> Vec<NodeDescPair> {
        let create_node_desc = |way: &RawWay, node_idx_in_way| {
            let node_id = way.node_ids[node_idx_in_way];
            let node = &entity_storages.node_storage.entities[node_id];
            NodeDesc::new(node_id, node.lat, node.lon)
        };
        self.way_refs
            .iter()
            .flat_map(|way_ref| {
                let way = &entity_storages.way_storage.entities[way_ref.way_id];
                (1..way.node_ids.len()).map(move |idx| {
                    NodeDescPair::new(
                        create_node_desc(way, idx - 1),
                        create_node_desc(way, idx),
                        way_ref.is_inner,
                    )
                })
            })
            .collect()
    }
}

pub(super) type Polygon = RawRefs;

#[derive(Default)]
pub(super) struct Multipolygon {
    pub(super) global_id: u64,
    pub(super) polygon_ids: RawRefs,
    pub(super) tags: RawTags,
}
