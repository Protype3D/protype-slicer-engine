use crate::{project::ProjectState, reducer};
use slicer_types::ProjectEvent;
use std::collections::BTreeSet;
use std::path::Path;

fn mesh_paths(project: &ProjectState) -> BTreeSet<&str> {
    let mut paths = BTreeSet::new();
    for object in project.objects.values() {
        paths.insert(object.source_path.as_str());
        paths.extend(object.fuzzy_facets.mesh_path.as_deref());
    }
    for part in project.parts.values() {
        paths.extend(
            part.sources
                .iter()
                .map(|source| source.source_path.as_str()),
        );
        paths.extend(part.bodies.iter().filter_map(|body| {
            body.geometry
                .as_ref()
                .map(|geometry| geometry.source_path.as_str())
        }));
    }
    paths.remove("");
    paths
}

pub(super) fn missing_new_meshes(current: &ProjectState, events: &[ProjectEvent]) -> Vec<String> {
    let mut target = current.clone();
    for event in events {
        reducer::reduce(&mut target, event);
    }
    mesh_paths(&target)
        .difference(&mesh_paths(current))
        .copied()
        .filter(|path| !Path::new(path).is_file())
        .map(str::to_owned)
        .collect()
}
