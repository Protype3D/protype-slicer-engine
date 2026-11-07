//! Local, deterministic physical engraving; never allocates cloud serials.
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use slicer_geometry::{
    serial::{engrave, support_blocker_facets, SerialLayout, SerialMark, FONT},
    TriangleMesh,
};

use crate::error::RpcError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    source_path: String,
    marking: SerialMark,
}

pub fn handle(params: Value) -> Result<Value, RpcError> {
    let p: Params = serde_json::from_value(params)
        .map_err(|e| RpcError::Project(format!("Invalid serial marking: {e}")))?;
    p.marking.validate().map_err(RpcError::Project)?;
    let metadata = std::fs::metadata(&p.source_path)
        .map_err(|e| RpcError::Project(format!("Cannot read source mesh: {e}")))?;
    if metadata.len() > 64 * 1024 * 1024 {
        return Err(RpcError::Project("Source mesh exceeds 64 MiB".into()));
    }
    let source = std::fs::read(&p.source_path)
        .map_err(|e| RpcError::Project(format!("Cannot read source mesh: {e}")))?;
    let mesh =
        TriangleMesh::from_stl_bytes(&source).map_err(|e| RpcError::Project(e.to_string()))?;
    let output = engrave(&mesh, &p.marking).map_err(RpcError::Project)?;
    let hash = hex::encode(Sha256::digest(&output));
    let root = slicer_state::object_cache_dir().map_err(|e| RpcError::Internal(e.to_string()))?;
    std::fs::create_dir_all(&root).map_err(|e| RpcError::Internal(e.to_string()))?;
    let path = root.join(format!("serial-{hash}.stl"));
    // Fully generated and validated before writing. Atomic rename prevents
    // importers from seeing a partially written mesh after a restart.
    if path.exists() {
        let cached = std::fs::read(&path).map_err(|e| RpcError::Internal(e.to_string()))?;
        if cached != output {
            return Err(RpcError::Internal("Serial mesh cache is corrupt".into()));
        }
        return Ok(
            json!({"mesh_path": path, "marked_mesh_sha256": hash, "font": FONT,
            "source_sha256": hex::encode(Sha256::digest(&source)),
            "width_mm": p.marking.width_mm(), "height_mm": p.marking.total_height_mm()}),
        );
    }
    let temporary = root.join(format!("serial-{}.tmp", ulid::Ulid::new()));
    std::fs::write(&temporary, &output).map_err(|e| RpcError::Internal(e.to_string()))?;
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(RpcError::Internal(error.to_string()));
    }
    Ok(
        json!({"mesh_path": path, "marked_mesh_sha256": hash, "font": FONT,
              "source_sha256": hex::encode(Sha256::digest(&source)),
              "width_mm": p.marking.width_mm(), "height_mm": p.marking.total_height_mm()}),
    )
}

pub const TEMPLATE_KEY: &str = "serial_marking";

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub part_id: String,
    #[serde(default, skip_serializing_if = "SerialLayout::is_row")]
    pub layout: SerialLayout,
    pub mesh_sha256: String,
    pub origin: [f64; 3],
    pub normal: [f64; 3],
    pub tangent: [f64; 3],
    pub height_mm: f64,
    pub depth_mm: f64,
    pub min_wall_mm: f64,
}
impl Template {
    pub fn mark(&self, serial: String) -> SerialMark {
        SerialMark {
            serial,
            layout: self.layout,
            origin: self.origin,
            normal: self.normal,
            tangent: self.tangent,
            height_mm: self.height_mm,
            depth_mm: self.depth_mm,
            min_wall_mm: self.min_wall_mm,
        }
    }
    pub fn validate(&self) -> Result<(), RpcError> {
        if !uuid_like(&self.part_id)
            || self.mesh_sha256.len() != 64
            || !self.mesh_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(RpcError::Project(
                "Invalid company part or source hash".into(),
            ));
        }
        self.mark("000000".into())
            .validate()
            .map_err(RpcError::Project)
    }
}
pub fn uuid_like(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
pub fn template(object: &slicer_state::ObjectRecord) -> Result<Option<Template>, RpcError> {
    object
        .overrides
        .get(TEMPLATE_KEY)
        .map(|v| {
            let s = v
                .as_str()
                .ok_or_else(|| RpcError::Project("Invalid serial placement".into()))?;
            let t: Template =
                serde_json::from_str(s).map_err(|e| RpcError::Project(e.to_string()))?;
            t.validate()?;
            Ok(t)
        })
        .transpose()
}
pub fn eligible(object: &slicer_state::ObjectRecord) -> Result<(), RpcError> {
    let m = object.transform;
    let x = [m[0], m[1], m[2]];
    let y = [m[4], m[5], m[6]];
    let z = [m[8], m[9], m[10]];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let cross = [
        x[1] * y[2] - x[2] * y[1],
        x[2] * y[0] - x[0] * y[2],
        x[0] * y[1] - x[1] * y[0],
    ];
    if [x, y, z].iter().any(|a| (dot(*a, *a) - 1.).abs() > 1e-4)
        || dot(x, y).abs() > 1e-4
        || dot(x, z).abs() > 1e-4
        || dot(y, z).abs() > 1e-4
        || (dot(cross, z) - 1.).abs() > 1e-4
        || m.iter().any(|v| !v.is_finite())
    {
        return Err(RpcError::Project(
            "Apply scaling or mirroring to the mesh before choosing a serial placement".into(),
        ));
    }

    if !object.operations.is_empty()
        || object.seam_facets_rle.is_some()
        || object.support_facets_rle.is_some()
        || object.filament_facets_rle.is_some()
        || object.overrides.contains_key("fan_facets_rle")
        || object.seam_target_point.is_some()
    {
        return Err(RpcError::Project("Serial engraving needs a model without facet paint or pending CAD operations; export the finished shape as STL first".into()));
    }
    Ok(())
}
pub fn load(path: &str) -> Result<(TriangleMesh, String), RpcError> {
    let meta = std::fs::metadata(path).map_err(|e| RpcError::Project(e.to_string()))?;
    if meta.len() > 64 * 1024 * 1024 {
        return Err(RpcError::Project("Source mesh exceeds 64 MiB".into()));
    }
    let bytes = std::fs::read(path).map_err(|e| RpcError::Project(e.to_string()))?;
    let mesh =
        TriangleMesh::from_stl_bytes(&bytes).map_err(|e| RpcError::Project(e.to_string()))?;
    Ok((mesh, hex::encode(Sha256::digest(&bytes))))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialSlice {
    pub run_id: String,
    pub serials: std::collections::BTreeMap<String, String>,
}

/// Personalized meshes live only in this immutable slice snapshot. The saved
/// project retains a placement template, so the next run gets fresh numbers.
pub fn personalize(
    mut snap: slicer_state::project::ProjectState,
    request: Option<SerialSlice>,
    mesh_paths: std::collections::BTreeMap<String, String>,
) -> Result<
    (
        slicer_state::project::ProjectState,
        Option<String>,
        Vec<Value>,
    ),
    RpcError,
> {
    let marked: Vec<_> = snap
        .objects
        .values()
        .filter(|o| o.overrides.contains_key(TEMPLATE_KEY))
        .map(|o| o.id.0.clone())
        .collect();
    if marked.is_empty() && request.is_none() {
        return Ok((snap, None, vec![]));
    }
    let req = request.ok_or_else(|| {
        RpcError::Project("Reserve new company serials before slicing this project".into())
    })?;
    if !uuid_like(&req.run_id)
        || marked.is_empty()
        || marked.len() != req.serials.len()
        || marked.iter().any(|id| !req.serials.contains_key(id))
    {
        return Err(RpcError::Project(
            "Serial manifest does not match this plate".into(),
        ));
    }
    let mut manifest = vec![];
    let mut seen = std::collections::HashSet::new();
    for id in marked {
        let obj = snap
            .objects
            .get_mut(&slicer_types::ObjectId(id.clone()))
            .unwrap();
        eligible(obj)?;
        let t = template(obj)?.unwrap();
        let serial = req.serials.get(&id).unwrap();
        if !seen.insert(serial.clone()) {
            return Err(RpcError::Project(
                "Every instance needs a unique serial".into(),
            ));
        }
        let path = mesh_paths
            .get(&id)
            .ok_or_else(|| RpcError::Project("Missing source mesh".into()))?;
        let (_, hash) = load(path)?;
        if hash != t.mesh_sha256 {
            return Err(RpcError::Project(
                "Model changed; select the serial placement again".into(),
            ));
        }
        let mark = t.mark(serial.clone());
        let output = handle(json!({"source_path":path,"marking":mark}))?;
        obj.source_path = output["mesh_path"].as_str().unwrap().to_string();
        obj.step_part_index = None;
        let (marked_mesh, _) = load(&obj.source_path)?;
        obj.support_facets_rle = Some(serial_support_mask(&marked_mesh, &mark));
        manifest.push(
            json!({"object_id":id,"serial":serial,"origin":t.origin,"normal":t.normal,
            "tangent":t.tangent,"height_mm":t.height_mm,"depth_mm":t.depth_mm,"font":FONT,"layout":t.layout,
            "marked_mesh_sha256":output["marked_mesh_sha256"]}),
        );
    }
    Ok((snap, Some(req.run_id), manifest))
}

/// Encode the existing support-paint protocol (value + little-endian u32 run).
/// This mask belongs solely to the personalized snapshot, never the source
/// project: its facet indices are valid only for this particular serial STL.
fn serial_support_mask(mesh: &TriangleMesh, mark: &SerialMark) -> String {
    use base64::Engine as _;
    let facets = support_blocker_facets(mesh, mark);
    let mut encoded = Vec::new();
    let mut start = 0;
    while start < facets.len() {
        let value = facets[start];
        let mut end = start + 1;
        while end < facets.len() && facets[end] == value {
            end += 1;
        }
        encoded.push(if value { 2 } else { 0 });
        encoded.extend_from_slice(&((end - start) as u32).to_le_bytes());
        start = end;
    }
    base64::engine::general_purpose::STANDARD.encode(encoded)
}

pub fn stamp_gcode(path: &str, run_id: &str) -> Result<(), std::io::Error> {
    let bytes = std::fs::read(path)?;
    let mut stamped = format!("; PROTYPE_PRINT_RUN:{run_id}\n").into_bytes();
    stamped.extend(bytes);
    std::fs::write(path, stamped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slicer_types::{Bounds, ObjectId, ProjectEvent, SettingValue};
    #[test]
    fn personalization_is_isolated_and_template_survives_project_save() {
        check_personalization(SerialLayout::Row);
        check_personalization(SerialLayout::Column);
    }
    fn check_personalization(layout: SerialLayout) {
        let dir = std::env::temp_dir().join(format!("serial-rpc-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.stl");
        let built = slicer_geometry::build_stl(&slicer_geometry::CsgNode::Box {
            size: [80., 30., 5.],
        })
        .unwrap();
        std::fs::write(&source, &built.stl).unwrap();
        let (mesh, hash) = load(source.to_str().unwrap()).unwrap();
        let choices = slicer_geometry::serial::candidates(&mesh, 3.5, 0.4, 1., layout).unwrap();
        let c = &choices[0];
        let t = Template {
            layout,
            part_id: "01234567-0123-4123-8123-0123456789ab".into(),
            mesh_sha256: hash.clone(),
            origin: c.origin,
            normal: c.normal,
            tangent: c.tangent,
            height_mm: 3.5,
            depth_mm: 0.4,
            min_wall_mm: 1.,
        };
        let state = slicer_state::StateLayer::new();
        let id = ObjectId("part".into());
        state.apply(ProjectEvent::ObjectAdded {
            object_id: id.clone(),
            name: "Part".into(),
            source_path: source.to_string_lossy().into_owned(),
            triangle_count: built.triangle_count as u64,
            bounds: Bounds {
                min: built.bounds_min,
                max: built.bounds_max,
            },
            source_hash: hash,
            step_part: None,
            split_part: None,
            transform: None,
        });
        state.apply(ProjectEvent::ObjectOverrideSet {
            object_id: id.clone(),
            key: TEMPLATE_KEY.into(),
            new_value: Some(SettingValue::String(serde_json::to_string(&t).unwrap())),
        });
        let project = dir.join("saved.3mf");
        slicer_state::persist::save(&project, &state.snapshot()).unwrap();
        let loaded = slicer_state::persist::load(&project).unwrap();
        assert_eq!(
            template(&loaded.objects[&id]).unwrap().unwrap().part_id,
            t.part_id
        );
        assert_eq!(
            template(&loaded.objects[&id]).unwrap().unwrap().layout,
            layout
        );
        assert!(personalize(loaded.clone(), None, Default::default()).is_err());
        let request = SerialSlice {
            run_id: t.part_id.clone(),
            serials: [("part".into(), "A7C42F".into())].into(),
        };
        let (snapshot, run, manifest) = personalize(
            loaded,
            Some(request),
            [("part".into(), source.to_string_lossy().into_owned())].into(),
        )
        .unwrap();
        assert_eq!(run.as_deref(), Some(t.part_id.as_str()));
        assert_eq!(manifest[0]["serial"], "A7C42F");
        assert_eq!(manifest[0]["layout"], serde_json::to_value(layout).unwrap());
        let mask = snapshot.objects[&id].support_facets_rle.as_ref().unwrap();
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(mask)
            .unwrap();
        let values: Vec<_> = raw
            .as_chunks::<5>()
            .0
            .iter()
            .flat_map(|run| {
                std::iter::repeat_n(
                    run[0],
                    u32::from_le_bytes(run[1..].try_into().unwrap()) as usize,
                )
            })
            .collect();
        let (marked_mesh, _) = load(&snapshot.objects[&id].source_path).unwrap();
        assert_eq!(values.len(), marked_mesh.triangles.len());
        assert!(values.contains(&2));
        assert!(values.contains(&0));
        assert!(!values.contains(&1));
        assert!(state.snapshot().objects[&id].support_facets_rle.is_none());
        assert_ne!(
            snapshot.objects[&id].source_path,
            state.snapshot().objects[&id].source_path
        );
        assert_eq!(std::fs::read(&source).unwrap(), built.stl);
        assert!(state.undo().is_some());
        assert!(template(&state.snapshot().objects[&id]).unwrap().is_none());
        let mut scaled = snapshot.objects[&id].clone();
        scaled.transform[0] = 0.5;
        assert!(eligible(&scaled).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
